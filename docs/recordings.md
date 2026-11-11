# Recordings

Built-in lightweight screencast recorder capturing JPEG frames and metadata, with an interactive scrubbing video player, IndexedDB background downloading, and client-side WebM export.

## Sound

with session audio (`services.browser-daemon.audio.enable`), a recording takes the session's sound too (`recordAudio`, `--record-audio`, on by default), paused and resumed with it; a recording that stayed silent keeps none. Start one without from the dashboard's recording dialog, the handoff's **Sound** toggle or `POST /api/sessions/<id>/record?audio=false`; while it runs, the sound switches on and off at once (the dashboard's REC badge or recording dialog, the handoff's **Sound** button, `POST /api/sessions/<id>/record-audio {"audio": false}`), and what was recorded silent plays back silent. A profile can say for itself whether its sessions are recorded and whether with sound (dashboard: profile menu → **Recording**; `browser-daemon profile set NAME --record on --record-audio off`; `PATCH /api/profiles/<name> {"record": true, "record_audio": false}`, `null` for the node's default). The player plays it in step with the frames (`m` mutes, ↑/↓ volume), and `GET /api/recordings/<id>/audio` serves it (Opus in WebM). **Save as WebM** encodes the recording on its node with ffmpeg, frames at their own times (VP9) and its sound: `GET /api/recordings/<id>/export.webm` (optional `?mode=concise` or `export-concise.webm`), `browser-daemon recording export ID --webm [--concise]`.

## Flight recorder

keeps the last seconds of a session's video in memory, like a dashcam, and saves them as an ordinary recording when something broke: the player, export, merge and sharing work on it. It is off by default and per session. Starting it makes the session produce frames even when nobody watches; stopping it (or the session ending) frees the buffer. It works on its own: an ordinary recording of the session may run at the same time. Dashboard: session menu → **Flight recorder**, then **Save last minute** and **Stop flight recorder**. `browser-daemon session flight-recorder SESSION start|stop|status|save [--seconds N]`, or `POST /api/sessions/<id>/flight-recorder {"action": "start", "seconds": 60}` (`action` is `start`, `stop`, `status` or `save`; `seconds` only with `start`, default 60, at most 300). It answers `{"active", "seconds", "buffered_secs", "recording_id"}`, `recording_id` being the recording the last save made.

The buffer holds frames of the last N seconds and at most 64 MiB per session; the oldest go first, so a busy page may hold less than N seconds. It keeps pictures only, no sound. A page that does not change keeps its last picture, so the saved recording shows it. A save keeps buffering. Saved recordings are titled "Flight recorder: ...", are ordinary recordings (cleanup limits apply, favorite them to keep them), and are not made while the buffer holds no frame yet (409). Unlike a normal recording, the buffer does not take the live view's frames: it takes its own screenshots (Chromium with its own display: grabs of it), as for an unwatched recording.

## Merging

select two or more recordings of a node in the dashboard and **Merge**: pick their order (drag or arrows) and whether the originals go once merged. They become one recording, one after the other, sound included. `browser-daemon recording merge ID... [--delete]`, `POST /api/recordings/merge {"ids": [...], "delete": false}`. The merge is a favorite if any source was, and its age counts from the merge; the originals stay if their sound could not be carried over. A recording being exported or merged is not removed by the cleanup or a delete (409). The Recordings tab's search field finds recordings by title, address, profile, node or date.

## Favorite recordings

the cleanup (age, count and storage limits) removes old recordings. Star one as a favorite and it is never removed: the star in the dashboard (recording menu, or select several), `K` in the TUI, `browser-daemon recording favorite ID...` (`unfavorite` undoes it; `keep` and `unkeep` still work), `POST /api/recordings/<id>/keep {"keep": true}` (the API keeps the old name). The star button next to the search shows only the favorites. Favorite (and still running) recordings do not count against the count and storage limits. No recording is a favorite unless you ask; deleting a favorite by hand still works (after a warning). Recordings, backups, history and profile directories are readable by the daemon's user only; `meta.json` is replaced atomically and rebuilt from `frames.jsonl` if a crash leaves it unreadable.

## Playback timing

The dashboard and public recording player default to **Concise**: each gap
between changed images lasts at most five seconds (at 1× speed). Select **Raw**
to watch with the original timing. Switching modes keeps the current frame.
In Concise mode, the seek bar, current timestamp, and total duration reflect
the concise playback timeline (matching concise exports); in Raw mode, they
reflect the original recording timeline. Sound jumps forward with skipped
still periods, so use Raw to hear uninterrupted audio on a static page.
Stored frames and audio retain their original timing; exports can be downloaded either concise (gaps capped at 5s) or raw.

Use **Copy link at current time** in the dashboard player menu, or
**Copy time link** in the public player, to link to a moment. Links carry `t`
in seconds of the original recording and the playback `mode` (concise or raw),
for example `#recordings/<id>?t=1563.000&mode=raw` or a public link ending in
`#t=1563.000&mode=raw`. Copying from an already public recording uses its public
link; other dashboard links require access to the dashboard.

Seeking into an unloaded section fetches a small window around the selected
time first. The dashboard pauses its background transfer for that request and
resumes afterward. Playback fetches following windows as needed until the
background download catches up. Shared players also request the selected window
with foreground priority. Scrubbing again cancels the previous seek request.
