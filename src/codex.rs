use std::path::PathBuf;

use rusqlite::Connection;

use crate::{
    Session, SessionProvider, TITLE_SEARCH_LIMIT, compact_title, display_path, limited_search_text,
};

pub(crate) fn load_sessions() -> Vec<Session> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    let db_path = home.join(".codex/state_5.sqlite");
    if !db_path.exists() {
        return Vec::new();
    }

    let Ok(conn) = Connection::open(db_path) else {
        return Vec::new();
    };
    let Ok(mut stmt) = conn.prepare(
        "select id, title, cwd, rollout_path, created_at, updated_at, tokens_used, first_user_message
         from threads
         where archived = 0
           and first_user_message <> ''
           and source in ('cli', 'vscode')
         order by updated_at_ms desc, id desc",
    ) else {
        return Vec::new();
    };

    let Ok(rows) = stmt.query_map([], |row| {
        let id: String = row.get(0)?;
        let title: String = row.get(1)?;
        let cwd: String = row.get(2)?;
        let rollout_path: String = row.get(3)?;
        let created_at: Option<i64> = row.get(4)?;
        let updated_at: i64 = row.get(5)?;
        let tokens: Option<i64> = row.get(6)?;
        let first_user_message: String = row.get(7).unwrap_or_default();
        Ok((
            id,
            title,
            cwd,
            rollout_path,
            created_at,
            updated_at,
            tokens.and_then(|value| u64::try_from(value).ok()),
            first_user_message,
        ))
    }) else {
        return Vec::new();
    };

    let mut sessions = Vec::new();
    for row in rows.flatten() {
        let (id, title, cwd, rollout_path, created_at, updated_at, tokens, first_user_message) =
            row;
        let cwd = PathBuf::from(cwd);
        let title = compact_title(&title, &first_user_message);
        let title_search =
            limited_search_text(format!("{title}\n{first_user_message}"), TITLE_SEARCH_LIMIT);
        let transcript_path = PathBuf::from(rollout_path);
        sessions.push(Session {
            id,
            provider: SessionProvider::Codex,
            cwd: cwd.clone(),
            cwd_display: display_path(&cwd),
            title,
            title_search,
            message_search: String::new(),
            message_turns: Vec::new(),
            transcript_path: Some(transcript_path),
            created_at,
            updated_at,
            tokens,
        });
    }
    sessions
}
