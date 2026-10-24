//! Go Live hover previews (v1.1, see `vox_proto::stream_preview`): the latest still picture of
//! each live, with the streamed app's name and icon, kept in memory only.
//!
//! - `PUT /api/me/stream-preview`: only while the caller is streaming a picture; at most one
//!   upload per [`MIN_UPLOAD_GAP`]; the JPEG and the PNG are checked (size, magic, header
//!   dimensions) but never decoded here: the host's PC is also somebody's gaming PC.
//! - `DELETE /api/me/stream-preview`: the streamer switched hover previews off; dropped at once.
//! - `GET /api/users/{id}/stream-preview`: only for people who may see the streamer's voice channel
//!   (the same check as every channel read, `Shared::can_see`); anybody else, or a live that ended,
//!   gets `404`. Viewers are rate limited too (they ask on hover, with a debounce and a cache).
//! - An entry is dropped when its live ends, the person leaves voice, moves or disconnects
//!   ([`StreamPreviews::forget`] from `state.rs`), and is never served for another channel than the
//!   one it was sent from.

use std::collections::HashMap;
use std::io::Cursor;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::Engine;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use vox_proto::*;

use crate::api::{ApiErr, AuthUser};
use crate::state::AppState;
use crate::util::hex;

/// Uploads closer together than this are refused (`429`); the app sends one every ~15-20 s, plus
/// one right after a source switch.
pub const MIN_UPLOAD_GAP: Duration = Duration::from_secs(3);
/// Hover lookups per viewer: a token bucket (rate per second, burst).
const FETCH_RATE_PER_SEC: f64 = 3.0;
const FETCH_BURST: f64 = 12.0;

#[derive(Clone)]
struct Entry {
    channel_id: ChannelId,
    jpeg: Option<Arc<[u8]>>,
    jpeg_at: Instant,
    app: Option<StreamApp>,
    /// (hash, PNG)
    icon: Option<(String, Arc<[u8]>)>,
    last_upload: Instant,
}

#[derive(Default)]
struct Inner {
    entries: HashMap<UserId, Entry>,
    fetch_buckets: HashMap<UserId, (f64, Instant)>,
}

/// The hub's previews (one per live). Cheap to lock: no I/O, no decoding under it.
#[derive(Default)]
pub struct StreamPreviews(Mutex<Inner>);

/// A checked upload, ready to keep.
#[derive(Debug, Default)]
pub struct Checked {
    jpeg: Option<Vec<u8>>,
    /// `Some` = replace the app (and its icon, `None` = no icon).
    app: Option<(StreamApp, Option<Vec<u8>>)>,
}

impl StreamPreviews {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The live of `user` ended (or they left, moved, disconnected): forget its picture.
    pub fn forget(&self, user: UserId) {
        let mut g = self.lock();
        if g.entries.remove(&user).is_some() {
            tracing::debug!(user, "stream preview dropped");
        }
    }

    /// Keep `c` as `user`'s preview for their live in `channel`. `Err(wait)` when too soon.
    fn put(&self, user: UserId, channel: ChannelId, c: Checked, now: Instant) -> Result<(), Duration> {
        let mut g = self.lock();
        if let Some(e) = g.entries.get(&user) {
            let since = now.duration_since(e.last_upload);
            if since < MIN_UPLOAD_GAP && e.channel_id == channel {
                return Err(MIN_UPLOAD_GAP - since);
            }
        }
        let e = g.entries.entry(user).or_insert_with(|| Entry { channel_id: channel, jpeg: None, jpeg_at: now, app: None, icon: None, last_upload: now });
        if e.channel_id != channel {
            // Moved while live: nothing of the other room's live is kept.
            *e = Entry { channel_id: channel, jpeg: None, jpeg_at: now, app: None, icon: None, last_upload: now };
        }
        e.last_upload = now;
        if let Some(j) = c.jpeg {
            e.jpeg = Some(j.into());
            e.jpeg_at = now;
        }
        if let Some((app, icon)) = c.app {
            e.app = Some(app);
            e.icon = icon.map(|png| (icon_hash(&png), png.into()));
        }
        Ok(())
    }

    fn get(&self, user: UserId) -> Option<Entry> {
        self.lock().entries.get(&user).cloned()
    }

    /// Only keep the entries of people who are still live in the channel they sent them from.
    fn retain(&self, live: impl Fn(UserId, ChannelId) -> bool) {
        self.lock().entries.retain(|u, e| live(*u, e.channel_id));
    }

    fn allow_fetch(&self, viewer: UserId, now: Instant) -> bool {
        let mut g = self.lock();
        if g.fetch_buckets.len() > 4096 {
            // (Bounded: the bucket of somebody who stopped asking is full again anyway.)
            g.fetch_buckets.retain(|_, (_, at)| now.duration_since(*at) < Duration::from_secs(10));
        }
        let b = g.fetch_buckets.entry(viewer).or_insert((FETCH_BURST, now));
        b.0 = (b.0 + now.duration_since(b.1).as_secs_f64() * FETCH_RATE_PER_SEC).min(FETCH_BURST);
        b.1 = now;
        if b.0 >= 1.0 {
            b.0 -= 1.0;
            true
        } else {
            false
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.lock().entries.len()
    }
}

/// 12 hex digits of the PNG's SHA-256.
fn icon_hash(png: &[u8]) -> String {
    hex(&Sha256::digest(png)[..6])
}

fn b64(s: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::STANDARD.decode(s.as_bytes()).ok()
}

/// What can be wrong with an upload (`400` / `413`).
#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    TooBig(&'static str),
    Bad(&'static str),
}

/// Check an upload without decoding any picture: base64, size caps, magic bytes, header
/// dimensions, the name.
pub fn check(up: StreamPreviewUpload) -> Result<Checked, Refused> {
    let jpeg = match up.jpeg.as_deref() {
        None => None,
        Some(s) => {
            if s.len() > STREAM_PREVIEW_MAX_JPEG * 4 / 3 + 4 {
                return Err(Refused::TooBig("picture too big"));
            }
            let raw = b64(s).ok_or(Refused::Bad("picture is not base64"))?;
            if raw.len() > STREAM_PREVIEW_MAX_JPEG {
                return Err(Refused::TooBig("picture too big"));
            }
            if !raw.starts_with(&[0xFF, 0xD8, 0xFF]) {
                return Err(Refused::Bad("picture is not a JPEG"));
            }
            let dims = image::ImageReader::with_format(Cursor::new(&raw), image::ImageFormat::Jpeg).into_dimensions().map_err(|_| Refused::Bad("unreadable picture"))?;
            if dims.0 == 0 || dims.1 == 0 || dims.0 > STREAM_PREVIEW_MAX_SIDE || dims.1 > STREAM_PREVIEW_MAX_SIDE {
                return Err(Refused::Bad("picture too large"));
            }
            Some(raw)
        }
    };
    let app = match up.app {
        None => None,
        Some(a) => {
            let name = StreamApp::clean_name(&a.name);
            if name.is_empty() {
                return Err(Refused::Bad("app name missing"));
            }
            let icon = match up.icon_png.as_deref() {
                None => None,
                Some(s) => {
                    if s.len() > STREAM_PREVIEW_MAX_ICON * 4 / 3 + 4 {
                        return Err(Refused::TooBig("icon too big"));
                    }
                    let raw = b64(s).ok_or(Refused::Bad("icon is not base64"))?;
                    if raw.len() > STREAM_PREVIEW_MAX_ICON {
                        return Err(Refused::TooBig("icon too big"));
                    }
                    if !raw.starts_with(b"\x89PNG\r\n\x1a\n") {
                        return Err(Refused::Bad("icon is not a PNG"));
                    }
                    let dims = image::ImageReader::with_format(Cursor::new(&raw), image::ImageFormat::Png).into_dimensions().map_err(|_| Refused::Bad("unreadable icon"))?;
                    if dims.0 == 0 || dims.1 == 0 || dims.0 > STREAM_ICON_MAX_SIDE || dims.1 > STREAM_ICON_MAX_SIDE {
                        return Err(Refused::Bad("icon too large"));
                    }
                    Some(raw)
                }
            };
            Some((StreamApp { name, ..a }, icon))
        }
    };
    Ok(Checked { jpeg, app })
}

/// Drop the previews of lives that ended in a way `state.rs` did not see (a race between an upload
/// and the end of the live).
fn prune(s: &AppState) {
    let members = s.voice_members();
    s.stream_previews.retain(|u, ch| members.iter().any(|m| m.user.id == u && m.streaming && !m.audio_only && m.channel_id == Some(ch)));
}

/// `PUT /api/me/stream-preview`
pub async fn put(State(s): State<AppState>, AuthUser(me): AuthUser, body: Result<Json<StreamPreviewUpload>, JsonRejection>) -> Result<Json<OkResponse>, ApiErr> {
    // (A body past `STREAM_PREVIEW_MAX_BODY` was cut off unread: `413`, as the contract says.)
    let Json(up) = body.map_err(|e| if e.status() == StatusCode::PAYLOAD_TOO_LARGE { ApiErr::new(StatusCode::PAYLOAD_TOO_LARGE, "upload too big") } else { e.into() })?;
    let Some(ch) = s.voice_of(me.id).filter(|m| m.streaming && !m.audio_only).and_then(|m| m.channel_id) else {
        return Err(ApiErr::new(StatusCode::FORBIDDEN, "not streaming"));
    };
    // v1.1 (E1): a live in an end-to-end encrypted room never leaves a picture with the hub (the
    // app sends only the name and icon there); one that does is refused whole.
    if up.jpeg.is_some() && s.channel(ch).is_some_and(|c| c.e2ee) {
        return Err(ApiErr::new(StatusCode::FORBIDDEN, ERR_STREAM_PREVIEW_E2EE));
    }
    let checked = match check(up) {
        Ok(c) => c,
        Err(Refused::TooBig(why)) => return Err(ApiErr::new(StatusCode::PAYLOAD_TOO_LARGE, why)),
        Err(Refused::Bad(why)) => return Err(ApiErr::new(StatusCode::BAD_REQUEST, why)),
    };
    let (bytes, app) = (checked.jpeg.as_ref().map_or(0, Vec::len), checked.app.is_some());
    if let Err(wait) = s.stream_previews.put(me.id, ch, checked, Instant::now()) {
        return Err(crate::api::too_many(wait));
    }
    prune(&s);
    tracing::debug!(user = %me.username, channel = ch, bytes, app, "stream preview stored");
    Ok(Json(OkResponse { ok: true }))
}

/// `DELETE /api/me/stream-preview`: the streamer turned hover previews off mid-live; the picture
/// goes at once (whether or not there was one).
pub async fn delete(State(s): State<AppState>, AuthUser(me): AuthUser) -> Json<OkResponse> {
    s.stream_previews.forget(me.id);
    Json(OkResponse { ok: true })
}

#[derive(Deserialize)]
pub struct GetQuery {
    icon: Option<String>,
}

/// `GET /api/users/{id}/stream-preview[?icon=<hash>]`
pub async fn get(State(s): State<AppState>, AuthUser(me): AuthUser, Path(id): Path<UserId>, Query(q): Query<GetQuery>) -> Result<Response, ApiErr> {
    let not_found = || ApiErr::new(StatusCode::NOT_FOUND, "no stream preview");
    if !s.stream_previews.allow_fetch(me.id, Instant::now()) {
        return Err(ApiErr::new(StatusCode::TOO_MANY_REQUESTS, "too many stream previews, try again shortly"));
    }
    // Live right now, with a picture, in a channel the caller may see (a private room without
    // access looks exactly like no live at all).
    let live = s.voice_of(id).filter(|m| m.streaming && !m.audio_only).and_then(|m| m.channel_id);
    let Some(ch) = live.filter(|ch| id == me.id || s.can_see(me.id, *ch)) else {
        return Err(not_found());
    };
    let Some(e) = s.stream_previews.get(id).filter(|e| e.channel_id == ch) else {
        return Err(not_found());
    };
    let enc = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
    let (icon_hash, icon_png) = match &e.icon {
        Some((h, png)) => (Some(h.clone()), (q.icon.as_deref() != Some(h.as_str())).then(|| enc(png))),
        None => (None, None),
    };
    let body = StreamPreview {
        user_id: id,
        channel_id: ch,
        // (Never a picture of an end-to-end encrypted room's live, whatever was kept.)
        jpeg: e.jpeg.as_deref().filter(|_| !s.channel(ch).is_some_and(|c| c.e2ee)).map(enc),
        age_ms: e.jpeg_at.elapsed().as_millis() as u64,
        app: e.app.clone(),
        icon_hash,
        icon_png,
    };
    Ok(([(header::CACHE_CONTROL, "private, no-store")], Json(body)).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jpeg(w: u32, h: u32) -> Vec<u8> {
        let mut out = Vec::new();
        let px = vec![90u8; (w * h * 3) as usize];
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 60).encode(&px, w, h, image::ExtendedColorType::Rgb8).unwrap();
        out
    }

    fn png(side: u32) -> Vec<u8> {
        use image::ImageEncoder;
        let mut out = Vec::new();
        let px = vec![200u8; (side * side * 4) as usize];
        image::codecs::png::PngEncoder::new(&mut out).write_image(&px, side, side, image::ExtendedColorType::Rgba8).unwrap();
        out
    }

    fn enc(b: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(b)
    }

    #[test]
    fn uploads_are_checked_without_decoding() {
        let ok = check(StreamPreviewUpload { jpeg: Some(enc(&jpeg(320, 180))), app: Some(StreamApp { name: " Jogo\u{1} ".into(), is_game: true, screen: false }), icon_png: Some(enc(&png(32))) }).unwrap();
        assert_eq!(ok.jpeg.as_ref().map(|j| &j[..3]), Some(&[0xFF, 0xD8, 0xFF][..]));
        let (app, icon) = ok.app.unwrap();
        assert_eq!((app.name.as_str(), app.is_game), ("Jogo", true));
        assert!(icon.is_some());
        // Too big a picture, the wrong format, too large dimensions, a bad name.
        assert_eq!(check(StreamPreviewUpload { jpeg: Some(enc(&vec![0xFF; STREAM_PREVIEW_MAX_JPEG + 1])), ..Default::default() }).unwrap_err(), Refused::TooBig("picture too big"));
        assert_eq!(check(StreamPreviewUpload { jpeg: Some(enc(&png(8))), ..Default::default() }).unwrap_err(), Refused::Bad("picture is not a JPEG"));
        assert_eq!(check(StreamPreviewUpload { jpeg: Some(enc(&jpeg(800, 100))), ..Default::default() }).unwrap_err(), Refused::Bad("picture too large"));
        assert_eq!(check(StreamPreviewUpload { jpeg: Some("not base64!".into()), ..Default::default() }).unwrap_err(), Refused::Bad("picture is not base64"));
        assert_eq!(check(StreamPreviewUpload { app: Some(StreamApp { name: " \n".into(), ..Default::default() }), ..Default::default() }).unwrap_err(), Refused::Bad("app name missing"));
        assert_eq!(check(StreamPreviewUpload { app: Some(StreamApp { name: "x".into(), ..Default::default() }), icon_png: Some(enc(&png(65))), ..Default::default() }).unwrap_err(), Refused::Bad("icon too large"));
        assert_eq!(check(StreamPreviewUpload { app: Some(StreamApp { name: "x".into(), ..Default::default() }), icon_png: Some(enc(&jpeg(8, 8))), ..Default::default() }).unwrap_err(), Refused::Bad("icon is not a PNG"));
    }

    #[test]
    fn one_entry_per_live_rate_limited_and_forgotten() {
        let p = StreamPreviews::default();
        let t0 = Instant::now();
        let app = |n: &str| Some((StreamApp { name: n.into(), ..Default::default() }, Some(png(16))));
        p.put(7, 1, Checked { jpeg: Some(jpeg(32, 18)), app: app("A") }, t0).unwrap();
        // Too soon: refused with the wait.
        assert!(p.put(7, 1, Checked { jpeg: Some(jpeg(32, 18)), app: None }, t0 + Duration::from_secs(1)).unwrap_err() <= MIN_UPLOAD_GAP);
        // A frame alone keeps the app and its icon.
        p.put(7, 1, Checked { jpeg: Some(jpeg(16, 16)), app: None }, t0 + MIN_UPLOAD_GAP).unwrap();
        let e = p.get(7).unwrap();
        assert_eq!(e.app.as_ref().map(|a| a.name.as_str()), Some("A"));
        assert!(e.icon.is_some() && e.jpeg.is_some());
        // A new app without an icon clears the old icon.
        p.put(7, 1, Checked { jpeg: None, app: Some((StreamApp { name: "Tela inteira".into(), screen: true, ..Default::default() }, None)) }, t0 + MIN_UPLOAD_GAP * 2).unwrap();
        assert!(p.get(7).unwrap().icon.is_none());
        // Moving to another room starts over at once (nothing of the old room's live is kept).
        p.put(7, 2, Checked { jpeg: None, app: None }, t0 + MIN_UPLOAD_GAP * 2).unwrap();
        let e = p.get(7).unwrap();
        assert!(e.jpeg.is_none() && e.app.is_none() && e.channel_id == 2);
        p.retain(|u, ch| u == 7 && ch == 2);
        assert_eq!(p.len(), 1);
        p.retain(|_, _| false);
        assert_eq!(p.len(), 0);
        p.put(8, 1, Checked::default(), t0).unwrap();
        p.forget(8);
        assert!(p.get(8).is_none());
    }

    #[test]
    fn hover_lookups_are_rate_limited() {
        let p = StreamPreviews::default();
        let t0 = Instant::now();
        let allowed = (0..40).filter(|_| p.allow_fetch(1, t0)).count();
        assert_eq!(allowed, FETCH_BURST as usize);
        assert!(p.allow_fetch(1, t0 + Duration::from_secs(1)), "refills");
        assert!(p.allow_fetch(2, t0), "per viewer");
    }
}
