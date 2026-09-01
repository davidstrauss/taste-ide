//! GNotifications: the moments that need the user, and the rule for when
//! they do.
//!
//! ENVIRONMENTS.md → "Gadget mode": *glancing is ambient; action gets a
//! notification.* The list is closed and short — a waiting permission
//! prompt, a turn ended, a failed environment build, an environment
//! flagging itself for review — because a notification the user learns to
//! dismiss is worse than none.
//!
//! **One rule governs all of them: never notify about the surface the
//! user is already looking at.** Looking at means both halves — the window
//! has focus AND the surface carrying the news is on screen. A permission
//! prompt in a background chat tab is invisible with the window focused,
//! so it notifies; the same prompt in the selected tab does not.
//!
//! Everything here is pure. [`decide`] takes a [`Moment`] and what the
//! user can see, and returns a [`Notice`] or nothing; the gio call that
//! follows is three lines in `chat.rs` and `window.rs`. [`Digest`] is the
//! other half of quiet: the same fact arriving twice — a state event
//! republished, a fleet re-assembled after an unrelated commit — is one
//! moment, and the first sighting of anything is a baseline rather than
//! news.

use std::collections::{BTreeMap, BTreeSet};

use gtk::gio::prelude::ApplicationExt;
use gtk::glib::object::IsA;
use taste_core::environment::EnvironmentId;

/// The application action a notification click activates. Its target is
/// the [`Surface`] string; the window registers the action and does the
/// routing.
pub const ACTION: &str = "surface";

/// Where a notice lands when the user clicks it.
///
/// The click has to arrive somewhere specific: "opens the IDE" is what
/// every other notification on the desktop already does, and it leaves the
/// user to find what wanted them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Surface {
    /// A chat, by the stable per-pane key the strip hands out.
    Chat(String),
    /// An environment's fleet row.
    Environment(EnvironmentId),
    /// An environment waiting for the user's judgment — its console
    /// detail, where the review band is.
    ///
    /// Separate from [`Surface::Environment`] even though both land on an
    /// environment, because they are different requests: one is "look at
    /// this row", the other is "there is a decision here for you". The
    /// routing is free to make them the same journey and the vocabulary
    /// must not pretend they are the same news.
    Review(EnvironmentId),
}

impl Surface {
    /// The action target the notification carries, and the string
    /// `window.rs` routes on. Deliberately flat: a GNotification target is
    /// a GVariant the desktop stores and may hand back after a restart, so
    /// it stays a string nothing has to deserialize into a live object.
    pub fn target(&self) -> String {
        match self {
            Surface::Chat(key) => format!("chat:{key}"),
            Surface::Environment(env) => format!("env:{env}"),
            Surface::Review(env) => format!("review:{env}"),
        }
    }

    pub fn parse(target: &str) -> Option<Self> {
        match target.split_once(':') {
            Some(("chat", key)) if !key.is_empty() => Some(Surface::Chat(key.to_string())),
            Some(("env", slug)) => EnvironmentId::parse(slug).ok().map(Surface::Environment),
            Some(("review", slug)) => EnvironmentId::parse(slug).ok().map(Surface::Review),
            _ => None,
        }
    }
}

/// Something happened that might need the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Moment {
    /// An agent is waiting on a permission answer. The one moment that is
    /// *blocking*: nothing proceeds until the user says yes or no.
    /// `detail` is the question, as the permission bar states it.
    PermissionRequested { chat: Chat, detail: String },
    /// A turn finished. Informational — it withdraws itself when the user
    /// comes back.
    TurnEnded { chat: Chat },
    /// The agent's connection dropped and the chat cannot continue.
    AgentDisconnected { chat: Chat, reason: String },
    /// The agent will not talk until the user signs in.
    ///
    /// Not in the design's list, because it predates it: this
    /// notification already existed and deleting a working one is not
    /// what this phase is for. It is the same shape as the rest — an
    /// agent blocked on the user — so it goes through the same rule
    /// rather than round the side.
    SignInRequired { chat: Chat },
    /// An environment's container failed to build or start.
    BuildFailed {
        env: EnvironmentId,
        name: String,
        message: String,
    },
    /// An environment says it is done and is waiting for a judgment.
    ///
    /// One per environment, not one per branch: the environment IS the
    /// unit of review (ENVIRONMENTS.md → "The review lifecycle"), so
    /// there is exactly one thing to go and look at and exactly one
    /// notification id it can occupy.
    ReadyForReview { env: EnvironmentId, name: String },
}

/// The chat a moment came from: a stable key to route back to, and a label
/// to say out loud.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chat {
    pub key: String,
    pub label: String,
}

/// What the user can currently see.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Attention {
    /// The window has focus. False and everything notifies.
    pub window_active: bool,
    /// The chat this moment is about is the selected tab. Only consulted
    /// for chat moments — a caller with no chat in hand leaves it false.
    pub chat_on_screen: bool,
    /// The fleet is the console's visible tab — which is where the
    /// review band is, and where an environment's own row already carries
    /// its accent rail. Consulted for review moments too, for exactly
    /// that reason: the news is on screen.
    pub fleet_on_screen: bool,
}

/// A notification, ready for gio.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    /// The gio notification id. **This is the coalescing mechanism**:
    /// sending twice under one id replaces rather than stacks, so an
    /// environment that fails, is retried and fails again occupies one
    /// slot in the shell — and so does a chat that asks two permission
    /// questions in a row. Scoped per chat and per environment, never
    /// global, because two chats each needing the user is two facts.
    pub id: String,
    pub title: String,
    pub body: String,
    pub surface: Surface,
    /// Whether this notice is still true once the user looks at the
    /// window. Informational ones (a turn ended) are withdrawn on focus;
    /// ones still awaiting an answer (a permission prompt) stay until the
    /// thing they are about is actually resolved.
    pub informational: bool,
}

/// The gio notification id for one kind of moment about one thing.
///
/// **The one place an id is spelled**, because two places would be two
/// places that have to agree: the sender and the withdrawer both name the
/// same notification, and a withdraw that misses leaves a stale card in the
/// shell saying an agent is still waiting for an answer it already got.
///
/// `scope` is the workspace key. Every id carries it because gio ids are
/// per-application and a taste-ide window is not an application — N windows
/// are open at once by design and they are all `taste-ide`. Without the
/// scope, two windows' first chats are both `chat-1`, the ordinals being
/// process-local counters; one window's "needs permission" then REPLACES
/// the other's in the shell, and the notification the user acts on belongs
/// to a conversation they were not asked about. A literal `taste-inbox`
/// was worse still — one id for every window on the machine.
pub fn notification_id(scope: &str, kind: &str, key: &str) -> String {
    if key.is_empty() {
        format!("taste-{scope}-{kind}")
    } else {
        format!("taste-{scope}-{kind}-{key}")
    }
}

/// Does this moment need the user, and how is it said?
///
/// The whole notification policy, in one function with no IO in it.
///
/// `scope` is the workspace key — see [`notification_id`]. It is a
/// parameter rather than a global so this stays a pure function of the
/// moment, the user's attention and the window it happened in.
pub fn decide(moment: &Moment, attention: &Attention, scope: &str) -> Option<Notice> {
    // The one rule. `on_screen` is the surface this particular moment
    // would send the user to; if they are already there, with the window
    // focused, the notification would tell them what they can see.
    let looking_at = |on_screen: bool| attention.window_active && on_screen;
    match moment {
        Moment::PermissionRequested { chat, detail } => {
            if looking_at(attention.chat_on_screen) {
                return None;
            }
            Some(Notice {
                id: notification_id(scope, "permission", &chat.key),
                title: format!("{} needs permission", chat.label),
                body: match first_line(detail) {
                    "" => "Waiting for your answer".to_string(),
                    question => question.to_string(),
                },
                surface: Surface::Chat(chat.key.clone()),
                // Stays put: the question is still unanswered whether or
                // not the user glanced at the window.
                informational: false,
            })
        }
        Moment::TurnEnded { chat } => {
            if looking_at(attention.chat_on_screen) {
                return None;
            }
            Some(Notice {
                id: notification_id(scope, "turn", &chat.key),
                title: format!("{} finished", chat.label),
                body: "The turn completed.".into(),
                surface: Surface::Chat(chat.key.clone()),
                informational: true,
            })
        }
        Moment::AgentDisconnected { chat, reason } => {
            if looking_at(attention.chat_on_screen) {
                return None;
            }
            Some(Notice {
                id: notification_id(scope, "disconnect", &chat.key),
                title: format!("{} disconnected", chat.label),
                body: match first_line(reason) {
                    "" => "The connection closed.".to_string(),
                    reason => reason.to_string(),
                },
                surface: Surface::Chat(chat.key.clone()),
                informational: true,
            })
        }
        Moment::SignInRequired { chat } => {
            if looking_at(attention.chat_on_screen) {
                return None;
            }
            Some(Notice {
                id: notification_id(scope, "auth", &chat.key),
                title: "Sign-in required".into(),
                body: format!("{} needs you to sign in", chat.label),
                surface: Surface::Chat(chat.key.clone()),
                informational: false,
            })
        }
        Moment::BuildFailed { env, name, message } => {
            if looking_at(attention.fleet_on_screen) {
                return None;
            }
            Some(Notice {
                id: notification_id(scope, "build", env.as_str()),
                title: format!("{name} failed to start"),
                body: first_line(message).to_string(),
                surface: Surface::Environment(env.clone()),
                // A failed build stays failed; seeing the window does not
                // fix it.
                informational: false,
            })
        }
        Moment::ReadyForReview { env, name } => {
            if looking_at(attention.fleet_on_screen) {
                return None;
            }
            Some(Notice {
                // One per environment: flagging twice is one thing
                // waiting, not two.
                id: notification_id(scope, "review", env.as_str()),
                title: format!("{name} is ready for review"),
                body: "It published its branch, stopped its container, and is waiting \
                       for you to merge or reject it."
                    .into(),
                surface: Surface::Review(env.clone()),
                // Informational: coming back to the window does not
                // decide anything, but the state is not lost — the row
                // keeps its mark and the console keeps its band.
                informational: true,
            })
        }
    }
}

/// What has already been said, so it is not said twice.
///
/// The event bus is coarse on purpose — the roster says "look again", a
/// state event republishes on every reconciliation pass — so the same fact
/// arrives repeatedly and only the *change* is news. Two rules:
///
/// - The first sighting of anything is a baseline, never a notification.
///   An IDE opened onto an environment that was already failed, or a
///   checkout that already had six branches waiting, must come up silent.
/// - A fact unchanged since last time is not news. A build that fails,
///   is retried and fails again is; the same failure republished is not.
#[derive(Debug, Default)]
pub struct Digest {
    /// Baselined per environment, not once for the digest: environments
    /// are picked up from their clones one at a time at startup, so
    /// "everything before the first event" is not a moment that exists.
    /// An environment created mid-session is silent on its first state
    /// too, which costs nothing — a fresh clone starts unconfigured and
    /// reaches `failed`, if it does, by a transition this reports.
    states: BTreeMap<EnvironmentId, String>,
    flagged: Option<BTreeSet<EnvironmentId>>,
}

impl Digest {
    /// One environment moved. Returns whether this is news.
    ///
    /// Takes the state slug rather than the state, so a build whose error
    /// message changes counts as one failure and a repeat of the same one
    /// does not.
    pub fn environment_moved(&mut self, env: &EnvironmentId, state: &str) -> bool {
        let previous = self.states.insert(env.clone(), state.to_string());
        previous.is_some_and(|previous| previous != state)
    }

    /// The fleet was re-assembled. Returns the environments that have
    /// flagged themselves since last time, in a stable order.
    ///
    /// The flag is persisted, so a restarted IDE sees a fleet that was
    /// already waiting — and must not announce it as if it had just
    /// happened. The first read is a baseline, exactly as it is for
    /// container states.
    pub fn newly_flagged(&mut self, flagged: &[EnvironmentId]) -> Vec<EnvironmentId> {
        let current: BTreeSet<EnvironmentId> = flagged.iter().cloned().collect();
        let previous = self.flagged.replace(current.clone());
        match previous {
            None => Vec::new(),
            Some(previous) => current.difference(&previous).cloned().collect(),
        }
    }
}

fn first_line(message: &str) -> &str {
    message.lines().next().unwrap_or(message).trim()
}

/// The gio half, and all of it: build the notification and hand it over.
///
/// The default action is what makes a notification worth clicking. It goes
/// to an application-level action (`app.surface`) because that is the only
/// scope the desktop can activate when the app is not running — the window
/// registers it, and routes the target to the surface that wanted the
/// user. Where the platform will not raise a window on request (most
/// Wayland compositors, without an activation token), the click still
/// lands the IDE on the right surface for when the user gets there.
pub fn send(app: &impl IsA<gtk::gio::Application>, notice: &Notice) {
    let notification = gtk::gio::Notification::new(&notice.title);
    notification.set_body(Some(&notice.body));
    notification.set_priority(if notice.informational {
        gtk::gio::NotificationPriority::Normal
    } else {
        // A blocking question and a failed build both leave the fleet
        // stuck until the user acts. HIGH keeps them on screen rather than
        // letting them time out unseen.
        gtk::gio::NotificationPriority::High
    });
    notification.set_default_action_and_target_value(
        &format!("app.{ACTION}"),
        Some(&gtk::glib::Variant::from(notice.surface.target())),
    );
    app.as_ref()
        .send_notification(Some(&notice.id), &notification);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// This window's workspace key. Any window's would do; what matters is
    /// that a second one differs.
    const SCOPE: &str = "a993aa1e2d9ad486";

    fn chat() -> Chat {
        Chat {
            key: "chat-2".into(),
            label: "Claude 2".into(),
        }
    }

    fn env(slug: &str) -> EnvironmentId {
        EnvironmentId::parse(slug).unwrap()
    }

    fn moments() -> Vec<Moment> {
        vec![
            Moment::PermissionRequested {
                chat: chat(),
                detail: "Write src/main.rs".into(),
            },
            Moment::TurnEnded { chat: chat() },
            Moment::AgentDisconnected {
                chat: chat(),
                reason: "the adapter exited\nstack trace…".into(),
            },
            Moment::BuildFailed {
                env: env("calm-1"),
                name: "the refactor".into(),
                message: "podman build: no such image\ndetail".into(),
            },
            Moment::SignInRequired { chat: chat() },
            Moment::ReadyForReview {
                env: env("spry-2"),
                name: "the refactor".into(),
            },
        ]
    }

    /// The decision table, whole. Rows are moments; columns are what the
    /// user can see. The only cells that suppress are the ones where the
    /// user is looking straight at the thing.
    #[test]
    fn nothing_notifies_about_the_surface_the_user_is_looking_at() {
        let everything_visible = Attention {
            window_active: true,
            chat_on_screen: true,
            fleet_on_screen: true,
        };
        for moment in moments() {
            assert_eq!(
                decide(&moment, &everything_visible, SCOPE),
                None,
                "{moment:?} interrupted a user already looking at it"
            );
            // The same surfaces, but the window is behind something else:
            // the user cannot see any of it.
            let unfocused = Attention {
                window_active: false,
                ..everything_visible
            };
            assert!(
                decide(&moment, &unfocused, SCOPE).is_some(),
                "{moment:?} stayed silent while the window was unfocused"
            );
            // Focused, but this moment's surface is not the one on screen.
            let elsewhere = Attention {
                window_active: true,
                ..Attention::default()
            };
            assert!(
                decide(&moment, &elsewhere, SCOPE).is_some(),
                "{moment:?} stayed silent while its surface was hidden"
            );
        }
    }

    /// A chat moment must consult the CHAT's visibility and nothing else:
    /// staring at the fleet does not mean seeing a permission prompt.
    #[test]
    fn each_moment_consults_only_its_own_surface() {
        let looking_at_fleet = Attention {
            window_active: true,
            fleet_on_screen: true,
            ..Attention::default()
        };
        assert!(decide(&moments()[0], &looking_at_fleet, SCOPE).is_some());
        // Both fleet-shaped moments consult the fleet: a failed build and
        // an environment asking for a judgment are both already on screen
        // when that tab is, and the row's own mark says so.
        assert_eq!(decide(&moments()[3], &looking_at_fleet, SCOPE), None);
        assert_eq!(decide(&moments()[5], &looking_at_fleet, SCOPE), None);

        let looking_at_chat = Attention {
            window_active: true,
            chat_on_screen: true,
            ..Attention::default()
        };
        assert_eq!(decide(&moments()[0], &looking_at_chat, SCOPE), None);
        assert!(decide(&moments()[3], &looking_at_chat, SCOPE).is_some());
        assert!(
            decide(&moments()[5], &looking_at_chat, SCOPE).is_some(),
            "staring at a conversation is not seeing the fleet behind it"
        );
    }

    /// Ids are the coalescing mechanism, so they have to be scoped right:
    /// per chat, per environment, per environment under review.
    #[test]
    fn ids_coalesce_per_chat_and_per_environment_never_globally() {
        let away = Attention::default();
        let notice = |moment| decide(&moment, &away, SCOPE).unwrap();

        let other = Chat {
            key: "chat-9".into(),
            label: "Claude 9".into(),
        };
        let ask = |chat| Moment::PermissionRequested {
            chat,
            detail: "Write src/main.rs".into(),
        };
        let a = notice(ask(chat()));
        let b = notice(ask(other.clone()));
        assert_ne!(a.id, b.id, "two chats each needing the user is two facts");
        // ...but one chat asking twice replaces itself.
        assert_eq!(a.id, notice(ask(chat())).id);
        // A permission prompt and a finished turn in the SAME chat are
        // different facts and must not overwrite each other.
        assert_ne!(a.id, notice(Moment::TurnEnded { chat: chat() }).id);

        let one = notice(Moment::BuildFailed {
            env: env("calm-1"),
            name: "one".into(),
            message: "boom".into(),
        });
        let two = notice(Moment::BuildFailed {
            env: env("spry-2"),
            name: "two".into(),
            message: "boom".into(),
        });
        assert_ne!(one.id, two.id);

        // The environment is the unit of review, so it is the unit of
        // notification too: one id per environment, and two of them do
        // not collide.
        let calm = notice(Moment::ReadyForReview {
            env: env("calm-1"),
            name: "calm-1".into(),
        });
        let spry = notice(Moment::ReadyForReview {
            env: env("spry-2"),
            name: "the refactor".into(),
        });
        assert_eq!(calm.id, notification_id(SCOPE, "review", "calm-1"));
        assert_ne!(calm.id, spry.id);
        // It is named by what the USER calls it, not by its slug.
        assert_eq!(spry.title, "the refactor is ready for review");
        assert_eq!(spry.surface, Surface::Review(env("spry-2")));
    }

    /// N windows are open at once by design, and gio notification ids are
    /// per APPLICATION — every taste-ide window is `taste-ide`. So the id
    /// has to carry the window, or two windows silently share one slot in
    /// the shell.
    ///
    /// The chat keys make it concrete: they are process-local ordinals, so
    /// the first chat in every window is `chat-1`. Before the scope, one
    /// window's "Claude needs permission" replaced the other's, and the
    /// notification the user clicked belonged to a conversation nobody had
    /// asked them about.
    #[test]
    fn two_windows_never_share_a_notification_slot() {
        const OTHER: &str = "65d80d2c48b3f0a1";
        let away = Attention::default();
        let here = |moment| decide(&moment, &away, SCOPE).unwrap();
        let there = |moment| decide(&moment, &away, OTHER).unwrap();

        // The same chat ordinal in two windows is two different chats.
        let ask = || Moment::PermissionRequested {
            chat: Chat {
                key: "chat-1".into(),
                label: "Claude".into(),
            },
            detail: "Write src/main.rs".into(),
        };
        assert_ne!(here(ask()).id, there(ask()).id);

        // The same environment slug in two windows is two environments —
        // every window has a `primary`.
        let build = || Moment::BuildFailed {
            env: env("primary"),
            name: "primary".into(),
            message: "boom".into(),
        };
        assert_ne!(here(build()).id, there(build()).id);

        // ...and two windows can hold environments with the same slug,
        // since slugs are minted per workspace.
        let ready = || Moment::ReadyForReview {
            env: env("calm-1"),
            name: "calm-1".into(),
        };
        assert_ne!(here(ready()).id, there(ready()).id);

        // Within one window everything still coalesces exactly as before.
        assert_eq!(here(ask()).id, here(ask()).id);
        assert_eq!(here(ready()).id, here(ready()).id);
    }

    /// The sender and the withdrawer name the same notification, or a
    /// dismissed prompt stays on screen claiming an agent is still waiting.
    /// One function spells the id, which is what makes that true.
    #[test]
    fn withdrawing_names_exactly_what_sending_named() {
        let away = Attention::default();
        let chat = Chat {
            key: "chat-4".into(),
            label: "Claude 4".into(),
        };
        let sent = decide(
            &Moment::PermissionRequested {
                chat: chat.clone(),
                detail: "rm -rf /".into(),
            },
            &away,
            SCOPE,
        )
        .unwrap();
        // This is the spelling `ChatPane::clear_notification` builds.
        assert_eq!(sent.id, notification_id(SCOPE, "permission", &chat.key));
        // The kinds are distinct, so withdrawing one leaves the others.
        for kind in ["turn", "disconnect", "auth"] {
            assert_ne!(sent.id, notification_id(SCOPE, kind, &chat.key));
        }
    }

    /// Clicking has to land somewhere specific, and the target survives
    /// the round trip through the desktop's GVariant.
    #[test]
    fn every_notice_routes_to_a_surface_that_round_trips() {
        let away = Attention::default();
        for moment in moments() {
            let notice = decide(&moment, &away, SCOPE).unwrap();
            assert!(!notice.title.is_empty() && !notice.body.is_empty());
            assert!(
                !notice.body.contains('\n'),
                "a notification body is one line: {:?}",
                notice.body
            );
            let target = notice.surface.target();
            assert_eq!(Surface::parse(&target).as_ref(), Some(&notice.surface));
        }
        assert_eq!(
            Surface::parse("env:calm-1"),
            Some(Surface::Environment(env("calm-1")))
        );
        assert_eq!(
            Surface::parse("review:calm-1"),
            Some(Surface::Review(env("calm-1")))
        );
        assert_ne!(
            Surface::Review(env("calm-1")).target(),
            Surface::Environment(env("calm-1")).target(),
            "\"look at this row\" and \"there is a decision here\" are different news"
        );
        // Junk from a stale desktop notification is dropped, not guessed.
        for bad in ["", "chat:", "env:NOT VALID", "env:", "whatever", "chat"] {
            assert_eq!(Surface::parse(bad), None, "{bad:?} was accepted");
        }
    }

    /// Which ones clear themselves when the user comes back: the ones that
    /// stop being true by being seen.
    #[test]
    fn only_the_informational_ones_are_withdrawn_on_focus() {
        let away = Attention::default();
        let informational = |moment| decide(&moment, &away, SCOPE).unwrap().informational;
        assert!(!informational(Moment::PermissionRequested {
            chat: chat(),
            detail: "Write src/main.rs".into(),
        }));
        assert!(!informational(Moment::BuildFailed {
            env: env("calm-1"),
            name: "one".into(),
            message: "boom".into(),
        }));
        assert!(informational(Moment::TurnEnded { chat: chat() }));
        assert!(informational(Moment::ReadyForReview {
            env: env("calm-1"),
            name: "calm-1".into(),
        }));
    }

    /// The digest is what keeps a coarse event bus from becoming a noisy
    /// desktop. Startup is silent; only changes speak.
    #[test]
    fn the_first_sighting_is_a_baseline_and_a_repeat_is_not_news() {
        let mut digest = Digest::default();
        // The IDE comes up onto an environment that is already failed.
        assert!(!digest.environment_moved(&env("calm-1"), "failed"));
        assert!(!digest.environment_moved(&env("spry-2"), "running"));
        // Republishing the same states says nothing.
        assert!(!digest.environment_moved(&env("calm-1"), "failed"));
        // A real transition does — including the retry that fails again.
        assert!(digest.environment_moved(&env("calm-1"), "building"));
        assert!(digest.environment_moved(&env("calm-1"), "failed"));
        // An environment created mid-session baselines on its own first
        // state and speaks from the transition after it, which is the one
        // that carries the news: a fresh clone starts unconfigured.
        assert!(!digest.environment_moved(&env("wry-3"), "config-detected"));
        assert!(digest.environment_moved(&env("wry-3"), "building"));
        assert!(digest.environment_moved(&env("wry-3"), "failed"));
    }

    /// The flag is persisted, so an IDE that has just started opens on a
    /// fleet that was already waiting. Announcing that as news would mean
    /// every restart notified about every environment the user had already
    /// seen — so the first read is a baseline, exactly as it is for
    /// container states.
    #[test]
    fn only_environments_flagged_since_the_last_look_are_news() {
        let mut digest = Digest::default();
        // Two were already waiting when the window opened.
        assert!(digest
            .newly_flagged(&[env("calm-1"), env("spry-2")])
            .is_empty());
        assert_eq!(
            digest.newly_flagged(&[env("calm-1"), env("spry-2"), env("wry-3")]),
            [env("wry-3")]
        );
        // Merging one away is not news, and does not resurrect the others.
        assert!(digest
            .newly_flagged(&[env("calm-1"), env("wry-3")])
            .is_empty());
        // Two at once, in a stable order.
        assert_eq!(
            digest.newly_flagged(&[env("wry-3"), env("calm-1"), env("zippy-9"), env("brisk-4"),]),
            [env("brisk-4"), env("zippy-9")]
        );
        // Nothing waiting on a primed digest is not a first sighting: the
        // one after it is news again.
        assert!(digest.newly_flagged(&[]).is_empty());
        assert_eq!(digest.newly_flagged(&[env("calm-1")]), [env("calm-1")]);
    }
}
