//! **Languages the editor highlights beyond GtkSourceView's own.**
//!
//! GtkSourceView compiles its language definitions into the library, and
//! has none for Typst (David, 2026-10-05: "Can I get syntax highlighting
//! for .typ?"), and its JSON one is too slow for a notebook's pictures
//! (`ipynb.lang`). Ours live in `data/language-specs/`, are compiled into the
//! binary, and are written at startup into a directory of the IDE's cache
//! that goes first on the language manager's search path — so they work
//! the same in a dev run, on the host, and in the Flatpak, with nothing to
//! install. Their styles map onto GtkSourceView's `def:` styles, so every
//! colour scheme colours them.

const LANGUAGES: &[(&str, &str)] = &[
    (
        "typst.lang",
        include_str!("../../../data/language-specs/typst.lang"),
    ),
    // A notebook's JSON, with its picture lines highlighted in one match
    // each rather than a character at a time.
    (
        "ipynb.lang",
        include_str!("../../../data/language-specs/ipynb.lang"),
    ),
];

/// Extensions GtkSourceView's own definitions do not claim, and the
/// language each is: CommonJS (`.cjs`) and its TypeScript forms (`.cts`,
/// `.mts`) are JavaScript and TypeScript, and 5.20's `js.lang` names only
/// `*.js` and `*.mjs` (David, 2026-10-25: "Add CJS syntax highlighting").
const ALIASES: &[(&str, &str)] = &[("cjs", "js"), ("cts", "typescript"), ("mts", "typescript")];

/// The language `path` is highlighted as: GtkSourceView's guess from its
/// name, else what [`ALIASES`] says its extension is.
pub fn guess(path: &std::path::Path) -> Option<sourceview5::Language> {
    let manager = sourceview5::LanguageManager::default();
    manager
        .guess_language(Some(path.to_string_lossy().as_ref()), None)
        .or_else(|| {
            let extension = path.extension()?.to_str()?.to_ascii_lowercase();
            let (_, id) = ALIASES.iter().find(|(ext, _)| *ext == extension)?;
            manager.language(id)
        })
}

/// Write the definitions where the language manager will look, and put
/// that directory first on its search path. Once, at startup, before
/// anything asks the manager for a language: it reads its search path the
/// first time it is asked. A definition already there with the same bytes
/// is not written again.
pub fn register() {
    let dir = gtk::glib::user_cache_dir().join("taste-ide/language-specs");
    for (name, text) in LANGUAGES {
        let file = dir.join(name);
        if std::fs::read_to_string(&file).ok().as_deref() == Some(*text) {
            continue;
        }
        if let Err(e) = std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&file, text)) {
            tracing::warn!("the {name} language definition was not written: {e}");
            return;
        }
    }
    let manager = sourceview5::LanguageManager::default();
    let ours = dir.to_string_lossy().into_owned();
    let mut path: Vec<String> = manager
        .search_path()
        .iter()
        .map(|entry| entry.to_string())
        .collect();
    if !path.contains(&ours) {
        path.insert(0, ours);
        let path: Vec<&str> = path.iter().map(String::as_str).collect();
        manager.set_search_path(&path);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_typ_file_is_typst() {
        crate::gtk_test::on_gtk_thread("languages: no display — skipped", || {
            super::register();
            let language = sourceview5::LanguageManager::default()
                .guess_language(Some("slides.typ"), None)
                .expect("a language for .typ");
            assert_eq!(language.id(), "typst");
        });
    }

    #[test]
    fn commonjs_and_its_typescript_forms_are_highlighted() {
        crate::gtk_test::on_gtk_thread("languages: no display — skipped", || {
            for (name, id) in [
                ("server.cjs", "js"),
                ("Config.CJS", "js"),
                ("types.cts", "typescript"),
                ("module.mts", "typescript"),
                ("plain.js", "js"),
            ] {
                let language = super::guess(std::path::Path::new(name))
                    .unwrap_or_else(|| panic!("no language for {name}"));
                assert_eq!(language.id(), id, "{name}");
            }
        });
    }
}
