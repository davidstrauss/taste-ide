//! **Questions an agent puts to the user, as a form** — an ACP form
//! elicitation, read into what the chat's question card draws and turned
//! back into the answer the agent reads.
//!
//! The case that matters is Claude Code's AskUserQuestion: the pinned
//! adapter sends one to a client that takes form elicitations, each question
//! a field of options with an optional "Other" box beside it, and reads the
//! accepted values back into the tool's answers. An MCP server's own form
//! comes the same way and gets the same card, its fields drawn by type.
//!
//! No GTK here: the card (`chat.rs`) holds the widgets, and this module is
//! the schema read in and the content written out, which is what tests can
//! pin down.

use std::collections::BTreeMap;

use agent_client_protocol::schema::v1::{
    CreateElicitationRequest, CreateElicitationResponse, ElicitationAcceptAction,
    ElicitationAction, ElicitationContentValue, ElicitationMode, ElicitationPropertySchema,
    ElicitationScope, EnumOption, MultiSelectItems,
};

/// The `_meta` key that marks a free-text field as the "Other" box of a
/// choice, and names that choice. Not agent-namespaced on purpose: the
/// adapters that bridge AskUserQuestion share it.
const CUSTOM_ANSWER_META: &str = "_askUserQuestionCustomAnswer";

/// One option of a choice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    /// What is sent back when it is picked.
    pub value: String,
    pub title: String,
    pub description: Option<String>,
}

/// What a field asks for.
#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    /// One of `choices`, or — with `other` naming its free-text companion —
    /// an answer of the user's own.
    One {
        choices: Vec<Choice>,
        other: Option<String>,
    },
    /// Any of `choices`, and with `other`, an answer of the user's own
    /// besides.
    Many {
        choices: Vec<Choice>,
        other: Option<String>,
    },
    Text {
        default: Option<String>,
    },
    Number {
        integer: bool,
        default: Option<f64>,
    },
    Toggle {
        default: bool,
    },
    /// A type this card does not draw; said, and left unanswered.
    Unsupported(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    pub key: String,
    /// A short name for it: AskUserQuestion's header ("Install shape").
    pub title: Option<String>,
    /// What it asks, when the form's message does not say it.
    pub description: Option<String>,
    pub required: bool,
    pub kind: Kind,
}

/// A form, as the card draws it.
#[derive(Debug, Clone, PartialEq)]
pub struct Form {
    /// The question, for a form of one; the form's heading otherwise.
    pub message: String,
    pub fields: Vec<Field>,
    /// The tool call it belongs to — AskUserQuestion's own step in the
    /// transcript — when the agent says.
    pub tool_call: Option<String>,
}

/// The form a request carries, or `None` for one that is not a form (a URL
/// to open, which is not offered, or a mode this client does not know).
pub fn form(request: &CreateElicitationRequest) -> Option<Form> {
    let ElicitationMode::Form(mode) = &request.mode else {
        return None;
    };
    let tool_call = match &mode.scope {
        ElicitationScope::Session(scope) => scope.tool_call_id.as_ref().map(|id| id.to_string()),
        _ => None,
    };
    let schema = &mode.requested_schema;
    let required: Vec<&str> = schema
        .required
        .iter()
        .flatten()
        .map(String::as_str)
        .collect();
    // The "Other" boxes first: each names the choice it belongs to, and is
    // drawn with it rather than as a field of its own.
    let mut companions: BTreeMap<String, String> = BTreeMap::new();
    for (key, property) in &schema.properties {
        if let ElicitationPropertySchema::String(string) = property {
            let named = string
                .meta
                .as_ref()
                .and_then(|meta| meta.get(CUSTOM_ANSWER_META))
                .and_then(|marker| marker.get("questionId"))
                .and_then(|id| id.as_str());
            if let Some(question) = named {
                if schema.properties.contains_key(question) {
                    companions.insert(question.to_string(), key.clone());
                }
            }
        }
    }
    let companion_keys: Vec<&String> = companions.values().collect();
    let mut fields = Vec::new();
    for (key, property) in &schema.properties {
        if companion_keys.contains(&key) {
            continue;
        }
        let other = companions.get(key).cloned();
        let (title, description, kind) = match property {
            ElicitationPropertySchema::String(string) => {
                let choices = string
                    .one_of
                    .as_ref()
                    .map(|options| options.iter().map(choice).collect::<Vec<_>>())
                    .or_else(|| {
                        string.enum_values.as_ref().map(|values| {
                            values
                                .iter()
                                .map(|value| Choice {
                                    value: value.clone(),
                                    title: value.clone(),
                                    description: None,
                                })
                                .collect()
                        })
                    });
                let kind = match choices {
                    Some(choices) => Kind::One { choices, other },
                    None => Kind::Text {
                        default: string.default.clone(),
                    },
                };
                (string.title.clone(), string.description.clone(), kind)
            }
            ElicitationPropertySchema::Array(array) => {
                let choices = match &array.items {
                    MultiSelectItems::Titled(items) => items.options.iter().map(choice).collect(),
                    MultiSelectItems::String(items) => items
                        .values
                        .iter()
                        .map(|value| Choice {
                            value: value.clone(),
                            title: value.clone(),
                            description: None,
                        })
                        .collect(),
                    _ => Vec::new(),
                };
                (
                    array.title.clone(),
                    array.description.clone(),
                    Kind::Many { choices, other },
                )
            }
            ElicitationPropertySchema::Number(number) => (
                number.title.clone(),
                number.description.clone(),
                Kind::Number {
                    integer: false,
                    default: number.default,
                },
            ),
            ElicitationPropertySchema::Integer(integer) => (
                integer.title.clone(),
                integer.description.clone(),
                Kind::Number {
                    integer: true,
                    default: integer.default.map(|n| n as f64),
                },
            ),
            ElicitationPropertySchema::Boolean(boolean) => (
                boolean.title.clone(),
                boolean.description.clone(),
                Kind::Toggle {
                    default: boolean.default.unwrap_or(false),
                },
            ),
            other => (None, None, Kind::Unsupported(property_type(other))),
        };
        fields.push(Field {
            key: key.clone(),
            title,
            description,
            required: required.contains(&key.as_str()),
            kind,
        });
    }
    Some(Form {
        message: request.message.clone(),
        fields,
        tool_call,
    })
}

fn choice(option: &EnumOption) -> Choice {
    Choice {
        value: option.value.clone(),
        title: option.title.clone(),
        description: option.description.clone().filter(|d| !d.trim().is_empty()),
    }
}

fn property_type(property: &ElicitationPropertySchema) -> String {
    serde_json::to_value(property)
        .ok()
        .and_then(|value| value.get("type")?.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".into())
}

/// What the user put in one field, as the card reads it back.
#[derive(Debug, Clone, PartialEq)]
pub enum Answer {
    /// The picked choice's value, if any, and the "Other" box's text.
    One(Option<String>, String),
    /// The picked values, and the "Other" box's text.
    Many(Vec<String>, String),
    Text(String),
    /// As typed; checked against the field when the form is sent.
    Number(String),
    Toggle(bool),
}

/// The accepted response for `answers`, keyed by field — or what is wrong
/// with them: a required field left empty, a number that is not one. A
/// field left empty is left out, which is what an unanswered question is.
pub fn accept(
    form: &Form,
    answers: &BTreeMap<String, Answer>,
) -> Result<CreateElicitationResponse, String> {
    let mut content: BTreeMap<String, ElicitationContentValue> = BTreeMap::new();
    for field in &form.fields {
        let name = field.title.as_deref().unwrap_or(&field.key);
        let given = match (&field.kind, answers.get(&field.key)) {
            (Kind::One { other, .. }, Some(Answer::One(picked, typed))) => {
                let typed = typed.trim();
                if let (Some(other), false) = (other, typed.is_empty()) {
                    content.insert(other.clone(), typed.into());
                }
                match picked {
                    Some(value) => {
                        content.insert(field.key.clone(), value.clone().into());
                        true
                    }
                    None => !typed.is_empty() && other.is_some(),
                }
            }
            (Kind::Many { other, .. }, Some(Answer::Many(picked, typed))) => {
                let typed = typed.trim();
                if let (Some(other), false) = (other, typed.is_empty()) {
                    content.insert(other.clone(), typed.into());
                }
                if !picked.is_empty() {
                    content.insert(field.key.clone(), picked.clone().into());
                }
                !picked.is_empty() || (!typed.is_empty() && other.is_some())
            }
            (Kind::Text { .. }, Some(Answer::Text(text))) => {
                let text = text.trim();
                if !text.is_empty() {
                    content.insert(field.key.clone(), text.into());
                }
                !text.is_empty()
            }
            (Kind::Number { integer, .. }, Some(Answer::Number(typed))) => {
                let typed = typed.trim();
                if typed.is_empty() {
                    false
                } else if *integer {
                    let n: i64 = typed
                        .parse()
                        .map_err(|_| format!("{name} wants a whole number"))?;
                    content.insert(field.key.clone(), n.into());
                    true
                } else {
                    let n: f64 = typed
                        .parse()
                        .map_err(|_| format!("{name} wants a number"))?;
                    content.insert(field.key.clone(), n.into());
                    true
                }
            }
            (Kind::Toggle { .. }, Some(Answer::Toggle(on))) => {
                content.insert(field.key.clone(), (*on).into());
                true
            }
            _ => false,
        };
        if field.required && !given {
            return Err(format!("{name} needs an answer"));
        }
    }
    Ok(CreateElicitationResponse::new(ElicitationAction::Accept(
        ElicitationAcceptAction::new().content(content),
    )))
}

/// The user skipped the questions: the agent is told so and goes on.
pub fn decline() -> CreateElicitationResponse {
    CreateElicitationResponse::new(ElicitationAction::Decline)
}

/// What was answered, a line per field, for the transcript: the field's
/// title and what was chosen or typed, by the choices' own titles.
pub fn summary(form: &Form, answers: &BTreeMap<String, Answer>) -> Vec<String> {
    let title_of = |choices: &[Choice], value: &str| {
        choices
            .iter()
            .find(|choice| choice.value == value)
            .map(|choice| choice.title.clone())
            .unwrap_or_else(|| value.to_string())
    };
    form.fields
        .iter()
        .filter_map(|field| {
            let said = match (&field.kind, answers.get(&field.key)?) {
                (Kind::One { choices, .. }, Answer::One(picked, typed)) => {
                    let mut parts: Vec<String> = picked
                        .iter()
                        .map(|value| title_of(choices, value))
                        .collect();
                    if !typed.trim().is_empty() {
                        parts.push(typed.trim().to_string());
                    }
                    parts.join(" — ")
                }
                (Kind::Many { choices, .. }, Answer::Many(picked, typed)) => {
                    let mut parts: Vec<String> = picked
                        .iter()
                        .map(|value| title_of(choices, value))
                        .collect();
                    if !typed.trim().is_empty() {
                        parts.push(typed.trim().to_string());
                    }
                    parts.join(", ")
                }
                (_, Answer::Text(text) | Answer::Number(text)) => text.trim().to_string(),
                (_, Answer::Toggle(on)) => if *on { "yes" } else { "no" }.to_string(),
                _ => String::new(),
            };
            if said.is_empty() {
                return None;
            }
            let name = field
                .title
                .clone()
                .or_else(|| field.description.clone())
                .unwrap_or_else(|| form.message.clone());
            Some(format!("{name}: {said}"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the pinned adapter (0.81.0) sends for an AskUserQuestion of one
    /// single-select question — its `askUserQuestionsToCreateRequest`, taken
    /// as the wire has it.
    fn ask_user_question() -> CreateElicitationRequest {
        serde_json::from_value(serde_json::json!({
            "mode": "form",
            "sessionId": "s",
            "toolCallId": "toolu_1",
            "message": "Should the exploitable stack replace the current clean install?",
            "requestedSchema": {
                "type": "object",
                "properties": {
                    "question_0": {
                        "type": "string",
                        "title": "Install shape",
                        "oneOf": [
                            {"const": "Replace install", "title": "Replace install",
                             "description": "One site, one code path."},
                            {"const": "Separate target", "title": "Separate target"}
                        ]
                    },
                    "question_0_custom": {
                        "type": "string",
                        "title": "Other",
                        "description": "Type your own answer (optional).",
                        "_meta": {"_askUserQuestionCustomAnswer":
                            {"questionId": "question_0", "isCustomAnswer": true}}
                    }
                }
            }
        }))
        .unwrap()
    }

    #[test]
    fn an_ask_user_question_reads_as_one_choice_with_its_other_box() {
        let form = form(&ask_user_question()).unwrap();
        assert_eq!(form.tool_call.as_deref(), Some("toolu_1"));
        assert_eq!(form.fields.len(), 1, "{form:?}");
        let field = &form.fields[0];
        assert_eq!(field.title.as_deref(), Some("Install shape"));
        let Kind::One { choices, other } = &field.kind else {
            panic!("{field:?}")
        };
        assert_eq!(other.as_deref(), Some("question_0_custom"));
        assert_eq!(choices.len(), 2);
        assert_eq!(
            choices[0].description.as_deref(),
            Some("One site, one code path.")
        );
        assert_eq!(choices[1].description, None);
    }

    #[test]
    fn a_pick_and_a_note_go_back_under_their_own_keys() {
        let form = form(&ask_user_question()).unwrap();
        let answers = BTreeMap::from([(
            "question_0".to_string(),
            Answer::One(Some("Separate target".into()), " keep both ".into()),
        )]);
        let response = serde_json::to_value(accept(&form, &answers).unwrap()).unwrap();
        assert_eq!(response["action"], "accept");
        assert_eq!(response["content"]["question_0"], "Separate target");
        assert_eq!(response["content"]["question_0_custom"], "keep both");
        assert_eq!(
            summary(&form, &answers),
            ["Install shape: Separate target — keep both"]
        );
    }

    #[test]
    fn nothing_picked_sends_nothing_for_that_question() {
        let form = form(&ask_user_question()).unwrap();
        let answers = BTreeMap::from([("question_0".to_string(), Answer::One(None, "".into()))]);
        let response = serde_json::to_value(accept(&form, &answers).unwrap()).unwrap();
        assert_eq!(response["content"], serde_json::json!({}));
        assert!(summary(&form, &answers).is_empty());
        assert_eq!(
            serde_json::to_value(decline()).unwrap()["action"],
            "decline"
        );
    }

    /// A multi-select question, and an MCP server's form of plain fields: a
    /// required one is held to, and a number is read as one.
    #[test]
    fn many_and_plain_fields_read_and_answer_by_type() {
        let request: CreateElicitationRequest = serde_json::from_value(serde_json::json!({
            "mode": "form",
            "sessionId": "s",
            "message": "Set it up",
            "requestedSchema": {
                "type": "object",
                "required": ["name"],
                "properties": {
                    "langs": {"type": "array", "title": "Languages",
                              "items": {"anyOf": [{"const": "rs", "title": "Rust"},
                                                  {"const": "py", "title": "Python"}]}},
                    "name": {"type": "string", "title": "Name"},
                    "count": {"type": "integer", "title": "Count"},
                    "dry": {"type": "boolean", "title": "Dry run", "default": true}
                }
            }
        }))
        .unwrap();
        let form = form(&request).unwrap();
        let kinds: Vec<(&str, bool)> = form
            .fields
            .iter()
            .map(|f| (f.key.as_str(), f.required))
            .collect();
        assert_eq!(
            kinds,
            [
                ("count", false),
                ("dry", false),
                ("langs", false),
                ("name", true)
            ]
        );
        assert!(matches!(
            form.fields[1].kind,
            Kind::Toggle { default: true }
        ));
        let mut answers = BTreeMap::from([
            (
                "langs".to_string(),
                Answer::Many(vec!["rs".into(), "py".into()], String::new()),
            ),
            ("count".to_string(), Answer::Number("three".into())),
            ("dry".to_string(), Answer::Toggle(false)),
            ("name".to_string(), Answer::Text(String::new())),
        ]);
        assert_eq!(
            accept(&form, &answers).unwrap_err(),
            "Count wants a whole number"
        );
        answers.insert("count".into(), Answer::Number("3".into()));
        assert_eq!(accept(&form, &answers).unwrap_err(), "Name needs an answer");
        answers.insert("name".into(), Answer::Text("demo".into()));
        let response = serde_json::to_value(accept(&form, &answers).unwrap()).unwrap();
        assert_eq!(
            response["content"],
            serde_json::json!({"count": 3, "dry": false, "langs": ["rs", "py"], "name": "demo"})
        );
        assert_eq!(
            summary(&form, &answers),
            [
                "Count: 3",
                "Dry run: no",
                "Languages: Rust, Python",
                "Name: demo"
            ]
        );
    }

    #[test]
    fn a_url_is_not_a_form() {
        let request: CreateElicitationRequest = serde_json::from_value(serde_json::json!({
            "mode": "url",
            "sessionId": "s",
            "elicitationId": "e",
            "url": "https://example.com",
            "message": "Sign in"
        }))
        .unwrap();
        assert!(form(&request).is_none());
    }
}
