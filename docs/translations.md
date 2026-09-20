# Translations

`qsTr()` in QML/JS sources, `app/translations/*.ts` sources, compiled
`*.qm` shipped in the RPM and loaded per locale at startup (`main.cpp`: full locale first, e.g.
`harbour-electric-eel_it_IT`, then bare language,
`harbour-electric-eel_it`; untranslated locales fall back to English).

## Scope

Translated: all QML/JS user-visible strings (pages, menus, dialogs,
command/category labels, model names). Not translated: protocol
identifiers sent to the car (command ids, argument names, enum values),
example placeholders, and Rust/C++ status and error strings (technical
diagnostics).

## Workflow

`tools/build-qm.sh` fetches Qt linguist tools from the
PySide6-Essentials wheel when missing (neither the host nor the SDK
ship them), runs `lupdate` over `app/qml`, merges the template into
per-language files (`tools/apply-translations.py`: keeps finished
translations, marks new strings unfinished), and compiles `*.qm` with
`lrelease`. `tools/build-qm.sh --check` fails if `app/translations/`
differs from committed state; CI runs it on every push, plus `qmllint`
syntax over `app/qml`.

## Adding a language

1. Copy `app/translations/harbour-electric-eel.ts` to
   `app/translations/harbour-electric-eel_<lang>.ts` (or add a fill
   script next to `tools/fill-it.py`).
2. Fill `<translation>` entries, removing `type="unfinished"`.
3. Run `tools/build-qm.sh` and commit the `.ts` and compiled `.qm`.

Shipped languages (39): bg bn cs da de el es et fi fr gu hi hu it kn
lt lv ml mr nb nl pa pl pt pt_BR ro ru sk sl sv ta te tr tt uk vi
zh_CN zh_HK zh_TW. Italian was written by hand;
the rest are LLM-generated from English — review by native speakers is
welcome (fix the `.ts`, rerun `tools/build-qm.sh`, commit `.ts`+`.qm`).
