//! Notes on sessions, profiles and recordings: short texts with the time they
//! were written, newest last, kept with what they are about.
//!
//! - A profile's live in its directory (`bd-notes.json`), so they travel with it
//!   (transfers, backups, exports) and survive clearing its data.
//! - A recording's live in its directory, and go when it is deleted.
//! - A session's live in `<state dir>/notes/<session id>.json`, and go when it ends.
//!
//! The overview carries each item's count and latest note (`NotesSummary`).

use super::*;

/// The notes file in a profile's or recording's directory.
pub const FILE: &str = "bd-notes.json";
/// The longest note, in characters.
const MAX_CHARS: usize = 4000;
/// The most notes one item keeps.
const MAX_NOTES: usize = 200;
/// How much of the latest note the overview carries.
const PREVIEW_CHARS: usize = 200;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum NoteKind {
    Session,
    Profile,
    Recording,
}

impl NoteKind {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "session" | "sessions" => Some(Self::Session),
            "profile" | "profiles" => Some(Self::Profile),
            "recording" | "recordings" => Some(Self::Recording),
            _ => None,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Note {
    pub id: Uuid,
    pub text: String,
    pub created_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edited_at: Option<u64>,
}

/// What the overview shows of an item's notes.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct NotesSummary {
    pub count: usize,
    /// The latest note, cut short.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_at: Option<u64>,
}

impl NotesSummary {
    fn of(notes: &[Note]) -> Option<Self> {
        let last = notes.last()?;
        let mut preview: String = last.text.chars().take(PREVIEW_CHARS).collect();
        if last.text.chars().count() > PREVIEW_CHARS {
            preview.push('…');
        }
        Some(Self {
            count: notes.len(),
            last: Some(preview),
            last_at: Some(last.edited_at.unwrap_or(last.created_at)),
        })
    }
}

/// A note's text: trimmed, not empty, not too long.
fn clean(text: &str) -> Result<String, &'static str> {
    let text = text.trim();
    if text.is_empty() {
        return Err("A note needs some text");
    }
    if text.chars().count() > MAX_CHARS {
        return Err("The note is too long (4000 characters at most)");
    }
    Ok(text.to_string())
}

async fn read(path: &Path) -> Vec<Note> {
    match fs::read(path).await {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            warn!(path = %path.display(), error = %e, "Ignoring unreadable notes");
            Vec::new()
        }),
        Err(_) => Vec::new(),
    }
}

async fn write(path: &Path, notes: &[Note]) -> std::io::Result<()> {
    if notes.is_empty() {
        return match fs::remove_file(path).await {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    }
    persist::write_atomic(path, &serde_json::to_vec_pretty(notes)?).await
}

/// One change at a time per file: read, change and write are a unit.
static LOCK: std::sync::LazyLock<Mutex<()>> = std::sync::LazyLock::new(|| Mutex::new(()));

fn sessions_dir(state: &AppState) -> PathBuf {
    state.config.state_dir().join("notes")
}

/// Where an item's notes are, if the item exists here.
async fn path_of(state: &AppState, kind: NoteKind, key: &str) -> Result<PathBuf, ApiError> {
    let gone = |what: &str| api_error(StatusCode::NOT_FOUND, format!("{what} not found"));
    match kind {
        NoteKind::Session => {
            let id: Uuid = key.parse().map_err(|_| gone("Session"))?;
            if !state.sessions.read().await.contains_key(&id) {
                return Err(gone("Session"));
            }
            Ok(sessions_dir(state).join(format!("{id}.json")))
        }
        NoteKind::Profile => Ok(lookup_profile(state, key).await?.dir.join(FILE)),
        NoteKind::Recording => {
            let id: Uuid = key.parse().map_err(|_| gone("Recording"))?;
            let dir = state.recorder_config.recordings_dir.join(id.to_string());
            if !fs::try_exists(dir.join("meta.json")).await.unwrap_or(false) {
                return Err(gone("Recording"));
            }
            Ok(dir.join(FILE))
        }
    }
}

fn kind_of(s: &str) -> Result<NoteKind, ApiError> {
    NoteKind::parse(s).ok_or_else(|| {
        api_error(
            StatusCode::NOT_FOUND,
            "Notes are for sessions, profiles and recordings",
        )
    })
}

/// A session ended: its notes go with it.
pub async fn forget_session(state: &AppState, id: Uuid) {
    let _ = fs::remove_file(sessions_dir(state).join(format!("{id}.json"))).await;
}

/// A session's notes summary, if it has notes.
pub async fn session_summary(state: &AppState, id: Uuid) -> Option<NotesSummary> {
    NotesSummary::of(&read(&sessions_dir(state).join(format!("{id}.json"))).await)
}

/// Every item of this node with notes: `"<kind>/<id>"` -> summary (profiles by id).
pub async fn summaries(state: &AppState) -> HashMap<String, NotesSummary> {
    let mut out = HashMap::new();
    let sessions: Vec<Uuid> = state.sessions.read().await.keys().copied().collect();
    for id in sessions {
        let notes = read(&sessions_dir(state).join(format!("{id}.json"))).await;
        if let Some(sum) = NotesSummary::of(&notes) {
            out.insert(format!("session/{id}"), sum);
        }
    }
    for p in profiles::list(&state.config.profiles_dir).await {
        let notes = read(&p.dir.join(FILE)).await;
        if let Some(sum) = NotesSummary::of(&notes) {
            out.insert(format!("profile/{}", p.meta.id), sum);
        }
    }
    if let Ok(mut dir) = fs::read_dir(&state.recorder_config.recordings_dir).await {
        while let Ok(Some(entry)) = dir.next_entry().await {
            let path = entry.path().join(FILE);
            if !fs::try_exists(&path).await.unwrap_or(false) {
                continue;
            }
            let notes = read(&path).await;
            if let Some(sum) = NotesSummary::of(&notes) {
                out.insert(
                    format!("recording/{}", entry.file_name().to_string_lossy()),
                    sum,
                );
            }
        }
    }
    out
}

// --------------------------------------------------------------------- API

/// `GET /api/notes`: the summaries of this node's notes.
pub(crate) async fn api_summaries(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    Ok(Json(summaries(&state).await))
}

/// `GET /api/notes/<kind>/<id>`: an item's notes, oldest first.
pub(crate) async fn api_list(
    AxumPath((kind, key)): AxumPath<(String, String)>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    let path = path_of(&state, kind_of(&kind)?, &key).await?;
    Ok(Json(read(&path).await))
}

#[derive(Deserialize, Serialize)]
pub(crate) struct NoteRequest {
    pub(crate) text: String,
}

fn io_error(e: std::io::Error) -> ApiError {
    api_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("Could not save the notes: {e}"),
    )
}

/// `POST /api/notes/<kind>/<id> {"text": ...}`: add a note.
pub(crate) async fn api_add(
    AxumPath((kind, key)): AxumPath<(String, String)>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<NoteRequest>,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    let kind = kind_of(&kind)?;
    let text = clean(&req.text).map_err(|e| api_error(StatusCode::BAD_REQUEST, e))?;
    let path = path_of(&state, kind, &key).await?;
    if kind == NoteKind::Session {
        persist::private_dir(&sessions_dir(&state))
            .await
            .map_err(io_error)?;
    }
    let _guard = LOCK.lock().await;
    let mut notes = read(&path).await;
    if notes.len() >= MAX_NOTES {
        return Err(api_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "This has as many notes as it can keep: delete some first",
        ));
    }
    let note = Note {
        id: Uuid::new_v4(),
        text,
        created_at: auth::now(),
        edited_at: None,
    };
    notes.push(note.clone());
    write(&path, &notes).await.map_err(io_error)?;
    Ok(Json(note))
}

/// `PUT /api/notes/<kind>/<id>/<note> {"text": ...}`: change a note.
pub(crate) async fn api_edit(
    AxumPath((kind, key, note)): AxumPath<(String, String, Uuid)>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<NoteRequest>,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    let text = clean(&req.text).map_err(|e| api_error(StatusCode::BAD_REQUEST, e))?;
    let path = path_of(&state, kind_of(&kind)?, &key).await?;
    let _guard = LOCK.lock().await;
    let mut notes = read(&path).await;
    let n = notes
        .iter_mut()
        .find(|n| n.id == note)
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, "Note not found"))?;
    n.text = text;
    n.edited_at = Some(auth::now());
    let edited = n.clone();
    write(&path, &notes).await.map_err(io_error)?;
    Ok(Json(edited))
}

/// `DELETE /api/notes/<kind>/<id>/<note>`.
pub(crate) async fn api_delete(
    AxumPath((kind, key, note)): AxumPath<(String, String, Uuid)>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    state.check_auth(&headers)?;
    let path = path_of(&state, kind_of(&kind)?, &key).await?;
    let _guard = LOCK.lock().await;
    let mut notes = read(&path).await;
    let before = notes.len();
    notes.retain(|n| n.id != note);
    if notes.len() == before {
        return Err(api_error(StatusCode::NOT_FOUND, "Note not found"));
    }
    write(&path, &notes).await.map_err(io_error)?;
    Ok(Json(serde_json::json!({ "status": "deleted", "id": note })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(text: &str, at: u64) -> Note {
        Note {
            id: Uuid::new_v4(),
            text: text.into(),
            created_at: at,
            edited_at: None,
        }
    }

    #[test]
    fn texts_are_trimmed_and_bounded() {
        assert_eq!(clean("  hi \n").unwrap(), "hi");
        assert!(clean("   \n ").is_err());
        assert!(clean(&"x".repeat(MAX_CHARS)).is_ok());
        assert!(clean(&"x".repeat(MAX_CHARS + 1)).is_err());
    }

    #[test]
    fn the_summary_is_the_latest_note_cut_short() {
        assert_eq!(NotesSummary::of(&[]), None);
        let long = "ä".repeat(PREVIEW_CHARS + 5);
        let sum = NotesSummary::of(&[note("first", 1), note(&long, 2)]).unwrap();
        assert_eq!(sum.count, 2);
        assert_eq!(sum.last_at, Some(2));
        let last = sum.last.unwrap();
        assert_eq!(last.chars().count(), PREVIEW_CHARS + 1);
        assert!(last.ends_with('…'));
    }

    #[test]
    fn kinds_parse_singular_and_plural() {
        assert_eq!(NoteKind::parse("profiles"), Some(NoteKind::Profile));
        assert_eq!(NoteKind::parse("recording"), Some(NoteKind::Recording));
        assert_eq!(NoteKind::parse("peers"), None);
    }

    #[tokio::test]
    async fn an_empty_list_removes_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        write(&path, &[note("a", 1)]).await.unwrap();
        assert_eq!(read(&path).await.len(), 1);
        write(&path, &[]).await.unwrap();
        assert!(!path.exists());
        write(&path, &[]).await.unwrap();
    }
}
