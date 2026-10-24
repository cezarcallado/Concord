//! Downloaded soundboard clips: files named `{id}-{hash}.wav` in one directory per server, and
//! the clips of the current server kept decoded in memory while in a voice channel, so a play
//! starts at once (v1.1: no disk read or WAV parse between the click and the first sample).
//!
//! The memory part is a least-recently-used list bounded by bytes ([`MEM_BUDGET`]): a full
//! soundboard of short clips fits, a server full of 5 s stereo clips keeps only the most recent
//! ones and reads the others from disk (off the caller's thread, see the backend).
//!
//! The hash is the clip's fingerprint from the sound list, so a file is valid forever: a renamed
//! sound keeps its file, a deleted-and-re-added one has a new id. A damaged file is deleted and
//! fetched again. [`SoundCache::retain`] drops files of sounds that no longer exist.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use parking_lot::Mutex;
use vox_proto::{parse_wav, wav_samples, SoundId};

/// Bytes of decoded clips kept in memory (mono 5 s is 480 KB, stereo 960 KB; a typical 1-2 s clip
/// is 100-200 KB, so a whole 50-sound board usually fits).
pub const MEM_BUDGET: usize = 16 * 1024 * 1024;

/// A clip as the mixer plays it: 48 kHz, interleaved, mono or stereo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoundPcm {
    pub channels: usize,
    pub samples: Arc<[i16]>,
}

impl SoundPcm {
    /// Parse a stored clip (the hub's canonical WAV).
    pub fn from_wav(bytes: &[u8]) -> Result<Self> {
        let info = parse_wav(bytes).map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(SoundPcm { channels: info.channels as usize, samples: Arc::from(wav_samples(bytes, &info)) })
    }

    pub fn frames(&self) -> usize {
        self.samples.len() / self.channels.max(1)
    }

    /// Memory the samples take.
    pub fn bytes(&self) -> usize {
        self.samples.len() * std::mem::size_of::<i16>()
    }

    pub fn duration_ms(&self) -> u32 {
        (self.frames() as u64 * 1000 / vox_proto::SOUND_RATE as u64) as u32
    }

    /// Interleaved stereo (the format of the mixer's alert sounds), for a local preview.
    pub fn to_stereo(&self) -> Arc<[i16]> {
        if self.channels == 2 {
            self.samples.clone()
        } else {
            Arc::from(self.samples.iter().flat_map(|&s| [s, s]).collect::<Vec<_>>())
        }
    }
}

type CacheKey = (SoundId, String);

#[derive(Default)]
struct Mem {
    /// Least recently used first.
    clips: VecDeque<(CacheKey, Arc<SoundPcm>)>,
    bytes: usize,
}

pub struct SoundCache {
    dir: PathBuf,
    mem: Mutex<Mem>,
    budget: usize,
}

impl SoundCache {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self::with_budget(dir, MEM_BUDGET)
    }

    /// A cache keeping at most `budget` bytes of decoded clips in memory.
    pub fn with_budget(dir: impl Into<PathBuf>, budget: usize) -> Self {
        Self { dir: dir.into(), mem: Mutex::new(Mem::default()), budget }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn file(&self, id: SoundId, hash: &str) -> PathBuf {
        // The hash comes from the hub: keep it from escaping the directory.
        let safe: String = hash.chars().filter(|c| c.is_ascii_alphanumeric()).take(64).collect();
        self.dir.join(format!("{id}-{safe}.wav"))
    }

    /// The clip, from memory or from disk; `None` when it was never downloaded (or the file is
    /// damaged, in which case it is removed).
    pub fn get(&self, id: SoundId, hash: &str) -> Option<Arc<SoundPcm>> {
        if let Some(pcm) = self.get_mem(id, hash) {
            return Some(pcm);
        }
        let key = (id, hash.to_string());
        let path = self.file(id, hash);
        let bytes = std::fs::read(&path).ok()?;
        match SoundPcm::from_wav(&bytes) {
            Ok(pcm) => Some(self.remember(key, pcm)),
            Err(e) => {
                tracing::warn!(id, error = %e, "cached sound is damaged; removing it");
                let _ = std::fs::remove_file(&path);
                None
            }
        }
    }

    /// The clip if it is decoded in memory; never touches the disk (safe on a thread that must
    /// not block). Marks it as recently used.
    pub fn get_mem(&self, id: SoundId, hash: &str) -> Option<Arc<SoundPcm>> {
        let mut mem = self.mem.lock();
        let i = mem.clips.iter().position(|(k, _)| k.0 == id && k.1 == hash)?;
        let hit = mem.clips.remove(i)?;
        let pcm = hit.1.clone();
        mem.clips.push_back(hit);
        Some(pcm)
    }

    /// Is the clip on disk or in memory (without decoding it)?
    pub fn has(&self, id: SoundId, hash: &str) -> bool {
        self.in_mem(id, hash) || self.file(id, hash).is_file()
    }

    fn in_mem(&self, id: SoundId, hash: &str) -> bool {
        self.mem.lock().clips.iter().any(|(k, _)| k.0 == id && k.1 == hash)
    }

    /// Decode into memory the clips of `list` that are on disk and not in memory yet, while the
    /// budget allows (blocking: run it off the async threads). Returns how many were loaded.
    pub fn warm(&self, list: &[(SoundId, String)]) -> usize {
        let mut loaded = 0;
        for (id, hash) in list {
            if self.mem.lock().bytes >= self.budget {
                break;
            }
            if !self.in_mem(*id, hash) && self.file(*id, hash).is_file() && self.get(*id, hash).is_some() {
                loaded += 1;
            }
        }
        loaded
    }

    /// Forget the decoded clips (the files stay): leaving the voice channel frees the memory.
    pub fn clear_mem(&self) {
        *self.mem.lock() = Mem::default();
    }

    /// Bytes of decoded clips in memory.
    pub fn mem_bytes(&self) -> usize {
        self.mem.lock().bytes
    }

    /// Keep a downloaded clip. Checked first: a hub that sends something that is not a sound
    /// leaves nothing behind.
    pub fn put(&self, id: SoundId, hash: &str, wav: &[u8]) -> Result<Arc<SoundPcm>> {
        let pcm = SoundPcm::from_wav(wav).context("the downloaded sound is not valid")?;
        std::fs::create_dir_all(&self.dir).with_context(|| format!("creating {}", self.dir.display()))?;
        let path = self.file(id, hash);
        let tmp = path.with_extension("part");
        std::fs::write(&tmp, wav).and_then(|_| std::fs::rename(&tmp, &path)).context("saving the sound")?;
        Ok(self.remember((id, hash.to_string()), pcm))
    }

    fn remember(&self, key: CacheKey, pcm: SoundPcm) -> Arc<SoundPcm> {
        let pcm = Arc::new(pcm);
        let mut mem = self.mem.lock();
        mem.forget(|k| *k == key);
        mem.bytes += pcm.bytes();
        mem.clips.push_back((key, pcm.clone()));
        // The oldest go first; the clip just used always stays (even one over the budget).
        while mem.bytes > self.budget && mem.clips.len() > 1 {
            if let Some((_, old)) = mem.clips.pop_front() {
                mem.bytes -= old.bytes();
            }
        }
        pcm
    }

    /// Delete the files (and memory) of every clip not in `keep` (the current sound list).
    pub fn retain(&self, keep: &[(SoundId, String)]) {
        self.mem.lock().forget(|k| !keep.contains(k));
        let Ok(entries) = std::fs::read_dir(&self.dir) else { return };
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().is_none_or(|x| x != "wav" && x != "part") {
                continue;
            }
            let wanted = keep.iter().any(|(id, hash)| self.file(*id, hash) == path);
            if !wanted {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
}

impl Mem {
    /// Drop the clips whose key matches.
    fn forget(&mut self, gone: impl Fn(&CacheKey) -> bool) {
        let mut bytes = self.bytes;
        self.clips.retain(|(k, pcm)| {
            let drop = gone(k);
            if drop {
                bytes -= pcm.bytes();
            }
            !drop
        });
        self.bytes = bytes;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vox_proto::canonical_wav;

    fn wav(v: i16, mono_frames: usize) -> Vec<u8> {
        canonical_wav(1, &vec![v; mono_frames])
    }

    #[test]
    fn put_get_and_survive_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let cache = SoundCache::new(dir.path().join("srv"));
        assert!(cache.get(1, "abc").is_none() && !cache.has(1, "abc"));
        let pcm = cache.put(1, "abc", &wav(7, 9600)).unwrap();
        assert_eq!((pcm.channels, pcm.frames(), pcm.duration_ms()), (1, 9600, 200));
        assert!(cache.has(1, "abc"));
        assert_eq!(cache.get(1, "abc").unwrap(), pcm);
        // A new process: from disk.
        let again = SoundCache::new(dir.path().join("srv"));
        assert!(again.has(1, "abc"));
        assert_eq!(again.get(1, "abc").unwrap().samples[0], 7);
        // Another hash of the same id is another clip.
        assert!(again.get(1, "def").is_none());
        // Mono is expanded to stereo for previews only.
        assert_eq!(pcm.to_stereo().len(), 2 * 9600);
    }

    #[test]
    fn bad_downloads_and_damaged_files_leave_nothing_usable() {
        let dir = tempfile::tempdir().unwrap();
        let cache = SoundCache::new(dir.path());
        assert!(cache.put(2, "x", b"not a wav").is_err());
        assert!(!cache.has(2, "x"));
        cache.put(3, "y", &wav(1, 9600)).unwrap();
        std::fs::write(dir.path().join("3-y.wav"), b"RIFFdamaged").unwrap();
        let fresh = SoundCache::new(dir.path());
        assert!(fresh.get(3, "y").is_none());
        assert!(!dir.path().join("3-y.wav").exists(), "the damaged file is removed");
        // A hostile hash cannot leave the directory.
        cache.put(4, "../../evil", &wav(1, 9600)).unwrap();
        assert!(dir.path().join("4-evil.wav").exists());
    }

    #[test]
    fn memory_is_bounded_by_bytes_least_recently_used_first() {
        let dir = tempfile::tempdir().unwrap();
        // Room for three 100 ms mono clips (9600 bytes each).
        let cache = SoundCache::with_budget(dir.path(), 3 * 9600);
        for id in 0..3 {
            cache.put(id, "h", &wav(id as i16, 4800)).unwrap();
        }
        assert_eq!(cache.mem_bytes(), 3 * 9600);
        // Using 0 makes 1 the oldest: the next clip pushes 1 out, not 0.
        assert!(cache.get_mem(0, "h").is_some());
        cache.put(3, "h", &wav(3, 4800)).unwrap();
        assert_eq!(cache.mem_bytes(), 3 * 9600, "never over the budget");
        assert!(cache.get_mem(1, "h").is_none() && cache.get_mem(0, "h").is_some() && cache.get_mem(3, "h").is_some());
        // A clip bigger than the whole budget is still kept (alone) while it is the newest.
        cache.put(4, "h", &wav(4, 48_000)).unwrap();
        assert_eq!(cache.mem_bytes(), 96_000);
        assert!(cache.get_mem(0, "h").is_none() && cache.get_mem(4, "h").is_some());
        // Evicted clips are still on disk: `get` decodes them again, `get_mem` never reads the disk.
        assert!(cache.get_mem(2, "h").is_none());
        assert_eq!(cache.get(2, "h").unwrap().samples[0], 2);
        // Leaving the channel frees the memory; warming brings the list back (within the budget).
        cache.clear_mem();
        assert_eq!(cache.mem_bytes(), 0);
        let list: Vec<_> = (0..4).map(|id| (id, "h".to_string())).collect();
        assert_eq!(cache.warm(&list), 3, "the budget stops it after three");
        assert_eq!(cache.mem_bytes(), 3 * 9600);
        assert!(cache.get_mem(0, "h").is_some() && cache.get_mem(2, "h").is_some());
        assert_eq!(cache.warm(&list), 0, "nothing more fits");
        // `retain` keeps the byte count right.
        cache.retain(&[(0, "h".into())]);
        assert_eq!(cache.mem_bytes(), 9600);
    }

    #[test]
    fn a_warm_cache_answers_without_the_disk() {
        let dir = tempfile::tempdir().unwrap();
        let cache = SoundCache::new(dir.path());
        cache.put(9, "h", &wav(5, 240_000)).unwrap(); // a 5 s clip
        let list = [(9, "h".to_string())];
        cache.clear_mem();
        assert!(cache.get_mem(9, "h").is_none(), "cold: only on disk");
        let t = std::time::Instant::now();
        let cold = cache.get(9, "h").unwrap();
        let cold_us = t.elapsed().as_micros();
        cache.clear_mem();
        assert_eq!(cache.warm(&list), 1);
        std::fs::remove_file(dir.path().join("9-h.wav")).unwrap();
        let t = std::time::Instant::now();
        let warm = cache.get_mem(9, "h").expect("in memory, the file is not needed");
        let warm_us = t.elapsed().as_micros();
        assert_eq!(warm, cold);
        println!("5 s clip: cold (disk read + parse) {cold_us} us, warm (memory) {warm_us} us");
        assert!(warm_us <= cold_us.max(1000));
    }

    #[test]
    fn memory_is_bounded_and_retain_prunes_files() {
        let dir = tempfile::tempdir().unwrap();
        let cache = SoundCache::with_budget(dir.path(), 12 * 9600);
        for id in 0..17 {
            cache.put(id, "h", &wav(id as i16, 4800)).unwrap();
        }
        assert_eq!(cache.mem.lock().clips.len(), 12);
        assert!(cache.get(0, "h").is_some(), "evicted from memory, still on disk");
        cache.retain(&[(0, "h".into()), (1, "h".into())]);
        let mut left: Vec<String> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        assert_eq!(left, ["0-h.wav", "1-h.wav"]);
        assert!(cache.get(7, "h").is_none());
    }
}
