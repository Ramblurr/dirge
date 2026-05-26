FTS5 'rebuild' uses old trigger formula — DELETE+INSERT SELECT for new content. env::set_var races in parallel tests → Mutex+EnvGuard. #[allow(dead_code)] module-level hides real dead code → targeted only. Schema: PRAGMA user_version, IF NOT EXISTS, handle dup cols. atomic_write_sync → Result<(), Error>. end_session() #[cfg(test)] only — calling in persist_turn_to_db() leaks session content into chat. Double session.add_message(User) causes dup messages in chat — event handler should render only, session mutation at input time.
§
## FTS5 formula migration: 'rebuild' doesn't work
External-content FTS5: `INSERT INTO fts(fts) VALUES('rebuild')` re-indexes using old trigger formula. To change indexed content (e.g. add tool_name to index), DELETE FROM fts then INSERT INTO fts SELECT id, new_formula FROM messages.
§
## #![allow(dead_code)] hides real dead code
Module-level suppression in agent_loop/mod.rs and lsp/mod.rs concealed ~50 genuinely unused items. Removing it revealed the true extent. Prefer targeted per-item annotations — even many are better than module-wide silence.
§
## env::set_var + parallel tests = flaky
`std::env::set_var` is global/unsafe/unsynchronized. Tests mutating same key race. Fix: static Mutex + RAII EnvGuard that clears on Drop (applied in dirge_paths.rs).
