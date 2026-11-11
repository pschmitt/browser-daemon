//! Opt-in human-like input for CDP clients (`/chromium?human_input=1`).
//!
//! The proxy reshapes a client's `Input.*` commands so they look less like a
//! bot: pointer moves along a curve before a click, text typed character by
//! character with uneven gaps, big wheel deltas in small steps. The planning
//! here is pure (a message in, a list of timed messages out); the proxy only
//! sleeps and sends them (see `bridge_websockets`).
//!
//! Injected messages use ids from a reserved range (`RESERVED_ID_BASE` and
//! up); their responses are recognised by that range and swallowed. The
//! client's own message is always the last step, so its response is returned
//! normally and message order is kept.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::{json, Value};

/// Injected messages count up from here. Chromium takes ids as 32-bit integers, so
/// the range sits just below that limit. Client ids start at 1 and count up one
/// by one; this is out of their reach.
pub(crate) const RESERVED_ID_BASE: u64 = 2_000_000_000;

/// A mouse move shorter than this is passed through as it is.
const MOVE_THRESHOLD_PX: f64 = 8.0;
/// Fewer wheel units than this in one event pass through.
const WHEEL_SPLIT_ABOVE: f64 = 150.0;
const WHEEL_STEP: f64 = 100.0;
const WHEEL_MAX_PARTS: usize = 10;
/// At most this many insertions for one `Input.insertText`, so a long fill
/// stays within the client's timeouts.
const TEXT_MAX_PARTS: usize = 100;

/// One message to send to the browser after waiting `delay`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Step {
    pub delay: Duration,
    pub text: String,
}

/// A small, fast, non-cryptographic generator (xorshift64*). Seedable, so the
/// tests are deterministic.
pub(crate) struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        // Zero would stay zero.
        Self(seed | 1)
    }

    pub fn from_entropy() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64);
        let uuid = uuid::Uuid::new_v4().as_u128() as u64;
        Self::new(nanos ^ uuid)
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// In [0, 1).
    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// In [lo, hi].
    pub fn range(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.unit()
    }

    pub fn millis(&mut self, lo: u64, hi: u64) -> Duration {
        Duration::from_millis(lo + self.next_u64() % (hi - lo + 1))
    }
}

/// The id of a response from the browser (`{"id":N,...`), without parsing the
/// whole message: the browser writes `id` first.
pub(crate) fn response_id(text: &str) -> Option<u64> {
    let rest = text.strip_prefix("{\"id\":")?;
    let end = rest.bytes().position(|b| !b.is_ascii_digit())?;
    rest[..end].parse().ok()
}

/// Is this a response to a message injected by us (to be dropped)?
pub(crate) fn is_injected_response(text: &str) -> bool {
    response_id(text).is_some_and(|id| id >= RESERVED_ID_BASE)
}

/// A query flag as `1`, `0`, `true`, `false`, `on`, `off`, `yes` or `no`
/// (`human_input=1` is what clients send; the default query parser takes only
/// `true` and `false`).
pub(crate) fn lenient_bool<'de, D>(d: D) -> Result<Option<bool>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    let Some(s) = <Option<String> as serde::Deserialize>::deserialize(d)? else {
        return Ok(None);
    };
    match s.to_ascii_lowercase().as_str() {
        "" => Ok(None),
        "1" | "true" | "on" | "yes" => Ok(Some(true)),
        "0" | "false" | "off" | "no" => Ok(Some(false)),
        other => Err(D::Error::custom(format!("not a boolean: {other}"))),
    }
}

/// A point on a cubic Bezier curve.
fn bezier(p: [(f64, f64); 4], t: f64) -> (f64, f64) {
    let u = 1.0 - t;
    let (a, b, c, d) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
    (
        a * p[0].0 + b * p[1].0 + c * p[2].0 + d * p[3].0,
        a * p[0].1 + b * p[1].1 + c * p[2].1 + d * p[3].1,
    )
}

fn smoothstep(t: f64) -> f64 {
    t * t * (3.0 - 2.0 * t)
}

/// How many steps and how long a pointer path over `dist` pixels takes:
/// 15 to 40 steps over 150 to 600 ms.
pub(crate) fn path_shape(dist: f64) -> (usize, u64) {
    let steps = (15.0 + dist / 25.0).clamp(15.0, 40.0) as usize;
    let ms = (150.0 + dist * 0.45).clamp(150.0, 600.0) as u64;
    (steps, ms)
}

/// The points of a curved pointer path from `from` (not included) to `to`
/// (the last point, exact). Random control points bend the curve, a smoothstep
/// gives ease-in-out pacing and the points in between jitter by a pixel or so.
pub(crate) fn mouse_path(rng: &mut Rng, from: (f64, f64), to: (f64, f64)) -> Vec<(f64, f64)> {
    let (dx, dy) = (to.0 - from.0, to.1 - from.1);
    let dist = dx.hypot(dy);
    let (steps, _) = path_shape(dist);
    // Control points a third and two thirds along, pushed sideways.
    let (nx, ny) = if dist > 0.0 {
        (-dy / dist, dx / dist)
    } else {
        (0.0, 0.0)
    };
    let bend = (dist * 0.2).min(120.0);
    let mut control = |frac: f64| {
        let side = rng.range(-bend, bend);
        let along = rng.range(-0.05, 0.05) * dist;
        (
            from.0 + dx * frac + nx * side + dx / dist.max(1.0) * along,
            from.1 + dy * frac + ny * side + dy / dist.max(1.0) * along,
        )
    };
    let p = [from, control(1.0 / 3.0), control(2.0 / 3.0), to];
    (1..=steps)
        .map(|i| {
            if i == steps {
                return to;
            }
            let (x, y) = bezier(p, smoothstep(i as f64 / steps as f64));
            (x + rng.range(-1.0, 1.0), y + rng.range(-1.0, 1.0))
        })
        .collect()
}

/// `text` cut into at most `max_parts` pieces of characters (one each unless
/// the text is longer than that), in order and without loss.
pub(crate) fn chunk_text(text: &str, max_parts: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let size = chars.len().div_ceil(max_parts.max(1)).max(1);
    chars.chunks(size).map(|c| c.iter().collect()).collect()
}

/// A wheel delta cut into parts of about `step`, at most `max_parts` of them,
/// summing to the delta exactly.
pub(crate) fn split_wheel(delta: f64, step: f64, max_parts: usize) -> Vec<f64> {
    let parts = ((delta.abs() / step).ceil() as usize).clamp(1, max_parts.max(1));
    let each = (delta / parts as f64).trunc();
    let mut out = vec![each; parts];
    out[parts - 1] = delta - each * (parts - 1) as f64;
    out
}

/// Per-connection state: the last pointer position of each CDP session, and the
/// next injected id.
pub(crate) struct HumanInput {
    rng: Rng,
    next_id: u64,
    pointer: HashMap<Option<String>, (f64, f64)>,
}

impl HumanInput {
    pub fn new() -> Self {
        Self::with_rng(Rng::from_entropy())
    }

    pub fn with_rng(rng: Rng) -> Self {
        Self {
            rng,
            next_id: RESERVED_ID_BASE,
            pointer: HashMap::new(),
        }
    }

    fn inject(&mut self, session: Option<&str>, method: &str, params: Value) -> String {
        let id = self.next_id;
        self.next_id += 1;
        let mut msg = json!({ "id": id, "method": method, "params": params });
        if let Some(s) = session {
            msg["sessionId"] = json!(s);
        }
        msg.to_string()
    }

    /// What to send for a client message. None: pass it through unchanged and
    /// at once. Some: send these in order, each after its delay; the last
    /// carries the client's id.
    pub fn plan(&mut self, text: &str) -> Option<Vec<Step>> {
        if !(text.contains("\"Input.dispatchMouseEvent\"")
            || text.contains("\"Input.insertText\"")
            || text.contains("\"Input.dispatchKeyEvent\""))
        {
            return None;
        }
        let mut msg: Value = serde_json::from_str(text).ok()?;
        // A client id in the reserved range would have its answer swallowed:
        // leave such a message alone.
        if msg["id"].as_u64().is_none_or(|id| id >= RESERVED_ID_BASE) {
            return None;
        }
        let session = msg["sessionId"].as_str().map(str::to_string);
        match msg["method"].as_str()? {
            "Input.dispatchMouseEvent" => self.plan_mouse(&mut msg, session),
            "Input.insertText" => self.plan_text(&mut msg, session),
            "Input.dispatchKeyEvent" => {
                let delay = if self.rng.unit() < 0.04 {
                    self.rng.millis(120, 300)
                } else {
                    self.rng.millis(8, 45)
                };
                Some(vec![Step {
                    delay,
                    text: text.to_string(),
                }])
            }
            _ => None,
        }
    }

    fn plan_mouse(&mut self, msg: &mut Value, session: Option<String>) -> Option<Vec<Step>> {
        let params = &msg["params"];
        let (x, y) = (params["x"].as_f64()?, params["y"].as_f64()?);
        let kind = params["type"].as_str()?.to_string();
        let buttons = params["buttons"].as_u64().unwrap_or(0);
        let last = self.pointer.insert(session.clone(), (x, y));
        match kind.as_str() {
            "mouseWheel" => {
                let (dx, dy) = (
                    params["deltaX"].as_f64().unwrap_or(0.0),
                    params["deltaY"].as_f64().unwrap_or(0.0),
                );
                if dx.abs().max(dy.abs()) <= WHEEL_SPLIT_ABOVE {
                    return None;
                }
                let xs = split_wheel(dx, WHEEL_STEP, WHEEL_MAX_PARTS);
                let ys = split_wheel(dy, WHEEL_STEP, WHEEL_MAX_PARTS);
                let n = xs.len().max(ys.len());
                let mut steps = Vec::with_capacity(n);
                for i in 0..n {
                    let (px, py) = (
                        xs.get(i).copied().unwrap_or(0.0),
                        ys.get(i).copied().unwrap_or(0.0),
                    );
                    let mut p = msg["params"].clone();
                    p["deltaX"] = json!(px);
                    p["deltaY"] = json!(py);
                    let delay = self.rng.millis(10, 40);
                    let text = if i + 1 == n {
                        msg["params"] = p;
                        msg.to_string()
                    } else {
                        self.inject(session.as_deref(), "Input.dispatchMouseEvent", p)
                    };
                    steps.push(Step { delay, text });
                }
                Some(steps)
            }
            "mousePressed" | "mouseMoved" => {
                // A drag (buttons held) keeps its own moves.
                if kind == "mouseMoved" && buttons != 0 {
                    return None;
                }
                let from = last
                    .unwrap_or_else(|| (self.rng.range(40.0, 700.0), self.rng.range(40.0, 400.0)));
                let dist = (x - from.0).hypot(y - from.1);
                if dist <= MOVE_THRESHOLD_PX {
                    return None;
                }
                let (_, ms) = path_shape(dist);
                let mut path = mouse_path(&mut self.rng, from, (x, y));
                if kind == "mouseMoved" {
                    // The client's own move is the last point.
                    path.pop();
                }
                let each = Duration::from_micros(ms * 1000 / (path.len().max(1) as u64 + 1));
                let mut steps = Vec::with_capacity(path.len() + 1);
                for (px, py) in path {
                    let mut p = json!({"type": "mouseMoved", "x": px, "y": py,
                        "button": "none", "buttons": 0});
                    for key in ["modifiers", "pointerType"] {
                        if !msg["params"][key].is_null() {
                            p[key] = msg["params"][key].clone();
                        }
                    }
                    steps.push(Step {
                        delay: each,
                        text: self.inject(session.as_deref(), "Input.dispatchMouseEvent", p),
                    });
                }
                // A moment of hovering before the click.
                let dwell = if kind == "mousePressed" {
                    self.rng.millis(20, 90)
                } else {
                    each
                };
                steps.push(Step {
                    delay: dwell,
                    text: msg.to_string(),
                });
                Some(steps)
            }
            // Releases and the rest only update the position, done above.
            _ => None,
        }
    }

    fn plan_text(&mut self, msg: &mut Value, session: Option<String>) -> Option<Vec<Step>> {
        let text = msg["params"]["text"].as_str()?.to_string();
        if text.chars().count() < 2 {
            return None;
        }
        let mut parts = chunk_text(&text, TEXT_MAX_PARTS);
        let last = parts.pop()?;
        let mut steps = Vec::with_capacity(parts.len() + 1);
        for (i, part) in parts.into_iter().enumerate() {
            steps.push(Step {
                delay: self.typing_gap(i),
                text: self.inject(
                    session.as_deref(),
                    "Input.insertText",
                    json!({"text": part}),
                ),
            });
        }
        msg["params"]["text"] = json!(last);
        steps.push(Step {
            delay: self.typing_gap(steps.len()),
            text: msg.to_string(),
        });
        Some(steps)
    }

    /// 30 to 140 ms between characters, now and then a longer pause (not
    /// before the first one).
    fn typing_gap(&mut self, index: usize) -> Duration {
        if index > 0 && self.rng.unit() < 0.05 {
            self.rng.millis(250, 700)
        } else {
            self.rng.millis(30, 140)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rng() -> Rng {
        Rng::new(0x1234_5678_9abc_def1)
    }

    fn parse(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn path_ends_exactly_and_step_count_is_bounded() {
        for (from, to) in [
            ((0.0, 0.0), (10.0, 0.0)),
            ((10.0, 10.0), (400.0, 300.0)),
            ((0.0, 0.0), (3000.0, 2000.0)),
        ] {
            let path = mouse_path(&mut rng(), from, to);
            assert!((15..=40).contains(&path.len()), "{}", path.len());
            assert_eq!(*path.last().unwrap(), to);
        }
    }

    #[test]
    fn path_progresses_towards_the_target() {
        let (from, to) = ((0.0, 0.0), (500.0, 200.0));
        let path = mouse_path(&mut rng(), from, to);
        let dist = |p: (f64, f64)| (p.0 - to.0).hypot(p.1 - to.1);
        // Roughly monotone: never moves back by more than a bounded amount.
        let mut best = dist(from);
        for p in &path {
            assert!(dist(*p) <= best + 60.0, "went back: {p:?}");
            best = best.min(dist(*p));
        }
        assert!(dist(path[path.len() / 2]) < dist(from));
    }

    #[test]
    fn path_to_the_same_point_does_not_panic() {
        let path = mouse_path(&mut rng(), (5.0, 5.0), (5.0, 5.0));
        assert_eq!(*path.last().unwrap(), (5.0, 5.0));
    }

    #[test]
    fn path_shape_stays_in_bounds() {
        for d in [0.0, 50.0, 500.0, 1e6] {
            let (s, ms) = path_shape(d);
            assert!((15..=40).contains(&s));
            assert!((150..=600).contains(&ms));
        }
    }

    #[test]
    fn chunks_rebuild_the_text() {
        assert_eq!(chunk_text("héllo", 100), vec!["h", "é", "l", "l", "o"]);
        let long: String = "ab".repeat(500);
        let parts = chunk_text(&long, 100);
        assert!(parts.len() <= 100);
        assert_eq!(parts.concat(), long);
        assert!(chunk_text("", 100).is_empty());
    }

    #[test]
    fn wheel_parts_sum_to_the_delta() {
        for d in [0.0, 100.0, -730.0, 5000.0, 123.0] {
            let parts = split_wheel(d, 100.0, 10);
            assert!(parts.len() <= 10 && !parts.is_empty());
            assert_eq!(parts.iter().sum::<f64>(), d);
        }
        assert_eq!(split_wheel(-730.0, 100.0, 10).len(), 8);
    }

    #[test]
    fn lenient_bool_takes_ones_and_words() {
        #[derive(serde::Deserialize)]
        struct Q {
            #[serde(default, deserialize_with = "lenient_bool")]
            v: Option<bool>,
        }
        for (s, want) in [
            ("1", Some(true)),
            ("false", Some(false)),
            ("0", Some(false)),
            ("", None),
        ] {
            let q: Q = serde_json::from_value(json!({ "v": s })).unwrap();
            assert_eq!(q.v, want, "{s}");
        }
        let q: Q = serde_json::from_value(json!({})).unwrap();
        assert_eq!(q.v, None);
        assert!(serde_json::from_value::<Q>(json!({ "v": "maybe" })).is_err());
    }

    #[test]
    fn response_ids() {
        assert_eq!(response_id("{\"id\":12,\"result\":{}}"), Some(12));
        assert_eq!(response_id("{\"method\":\"X\"}"), None);
        assert!(is_injected_response("{\"id\":2000000001,\"result\":{}}"));
        assert!(!is_injected_response("{\"id\":99,\"result\":{}}"));
        assert!(!is_injected_response(
            "{\"method\":\"Page.x\",\"params\":{\"id\":2000000001}}"
        ));
    }

    #[test]
    fn other_messages_pass_through() {
        let mut h = HumanInput::with_rng(rng());
        assert!(h
            .plan("{\"id\":1,\"method\":\"Page.navigate\",\"params\":{}}")
            .is_none());
        assert!(h.plan("not json Input.insertText").is_none());
    }

    #[test]
    fn click_is_preceded_by_a_path_and_keeps_the_client_id() {
        let mut h = HumanInput::with_rng(rng());
        let click = "{\"id\":7,\"sessionId\":\"S\",\"method\":\"Input.dispatchMouseEvent\",\
            \"params\":{\"type\":\"mousePressed\",\"x\":300,\"y\":200,\"button\":\"left\",\"clickCount\":1}}";
        let steps = h.plan(click).unwrap();
        let (last, injected) = steps.split_last().unwrap();
        assert_eq!(parse(&last.text), parse(click));
        assert!(injected.len() >= 15);
        let mut ids = std::collections::HashSet::new();
        for s in injected {
            let m = parse(&s.text);
            assert_eq!(m["sessionId"], "S");
            assert_eq!(m["params"]["type"], "mouseMoved");
            assert!(m["id"].as_u64().unwrap() >= RESERVED_ID_BASE);
            assert!(ids.insert(m["id"].as_u64().unwrap()));
        }
        let end = parse(&injected.last().unwrap().text);
        assert_eq!(
            (end["params"]["x"].as_f64(), end["params"]["y"].as_f64()),
            (Some(300.0), Some(200.0))
        );
        // The next click on the spot needs no path.
        assert!(h.plan(click).is_none());
    }

    #[test]
    fn pointer_is_tracked_per_session() {
        let mut h = HumanInput::with_rng(rng());
        let ev = |s: &str, x: u32| {
            format!(
                "{{\"id\":1,\"sessionId\":\"{s}\",\"method\":\"Input.dispatchMouseEvent\",\
                 \"params\":{{\"type\":\"mouseMoved\",\"x\":{x},\"y\":50}}}}"
            )
        };
        assert!(h.plan(&ev("A", 500)).is_some());
        assert!(h.plan(&ev("A", 503)).is_none());
        // Another session starts from elsewhere.
        assert!(h.plan(&ev("B", 503)).is_some());
    }

    #[test]
    fn drag_moves_pass_through() {
        let mut h = HumanInput::with_rng(rng());
        let m = "{\"id\":1,\"method\":\"Input.dispatchMouseEvent\",\
            \"params\":{\"type\":\"mouseMoved\",\"x\":900,\"y\":50,\"buttons\":1}}";
        assert!(h.plan(m).is_none());
    }

    #[test]
    fn moved_keeps_the_clients_final_move() {
        let mut h = HumanInput::with_rng(rng());
        let m = "{\"id\":3,\"method\":\"Input.dispatchMouseEvent\",\
            \"params\":{\"type\":\"mouseMoved\",\"x\":900,\"y\":500}}";
        let steps = h.plan(m).unwrap();
        assert_eq!(parse(&steps.last().unwrap().text)["id"], 3);
        assert!(steps.len() >= 15);
    }

    #[test]
    fn insert_text_becomes_one_insertion_per_character() {
        let mut h = HumanInput::with_rng(rng());
        let m = "{\"id\":5,\"method\":\"Input.insertText\",\"params\":{\"text\":\"hey!\"}}";
        let steps = h.plan(m).unwrap();
        assert_eq!(steps.len(), 4);
        let texts: Vec<String> = steps
            .iter()
            .map(|s| {
                parse(&s.text)["params"]["text"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(texts.concat(), "hey!");
        assert_eq!(parse(&steps[3].text)["id"], 5);
        for s in &steps[..3] {
            assert!(parse(&s.text)["id"].as_u64().unwrap() >= RESERVED_ID_BASE);
        }
        for s in &steps {
            assert!(s.delay >= Duration::from_millis(30));
        }
        // A single character is left alone.
        assert!(h
            .plan("{\"id\":6,\"method\":\"Input.insertText\",\"params\":{\"text\":\"a\"}}")
            .is_none());
    }

    #[test]
    fn long_text_is_capped() {
        let mut h = HumanInput::with_rng(rng());
        let text = "x".repeat(5000);
        let m = format!(
            "{{\"id\":5,\"method\":\"Input.insertText\",\"params\":{{\"text\":\"{text}\"}}}}"
        );
        assert!(h.plan(&m).unwrap().len() <= TEXT_MAX_PARTS);
    }

    #[test]
    fn key_events_get_a_gap_and_stay_unchanged() {
        let mut h = HumanInput::with_rng(rng());
        let m = "{\"id\":2,\"method\":\"Input.dispatchKeyEvent\",\"params\":{\"type\":\"keyDown\",\"key\":\"a\"}}";
        let steps = h.plan(m).unwrap();
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].text, m);
    }

    #[test]
    fn big_wheel_is_split_and_small_one_passes() {
        let mut h = HumanInput::with_rng(rng());
        let big = "{\"id\":4,\"method\":\"Input.dispatchMouseEvent\",\
            \"params\":{\"type\":\"mouseWheel\",\"x\":10,\"y\":10,\"deltaX\":0,\"deltaY\":800}}";
        let steps = h.plan(big).unwrap();
        assert_eq!(steps.len(), 8);
        let sum: f64 = steps
            .iter()
            .map(|s| parse(&s.text)["params"]["deltaY"].as_f64().unwrap())
            .sum();
        assert_eq!(sum, 800.0);
        assert_eq!(parse(&steps[7].text)["id"], 4);
        let small = "{\"id\":4,\"method\":\"Input.dispatchMouseEvent\",\
            \"params\":{\"type\":\"mouseWheel\",\"x\":10,\"y\":10,\"deltaX\":0,\"deltaY\":100}}";
        assert!(h.plan(small).is_none());
    }

    #[test]
    fn client_ids_in_the_reserved_range_are_left_alone() {
        let mut h = HumanInput::with_rng(rng());
        let m = "{\"id\":2000000005,\"method\":\"Input.insertText\",\"params\":{\"text\":\"hey\"}}";
        assert!(h.plan(m).is_none());
    }
}
