//! "Apoie o concord" (v1.1, D1): the Pix for donations, everything that is not drawing (the dialog
//! and the settings page are in `donate_ui.rs`).
//!
//! **The project's Pix goes in [`PIX`], and only there.** While it is `None` nothing of this exists
//! for the user: no popup, no section in Configurações, no usage counting, nothing saved.
//!
//! With a Pix set: the static BR Code (the "Pix copia e cola" text, [`br_code`]), its QR code
//! ([`qr_modules`]), and the rule of the two popups ([`popup_due`]): the first on the first entry
//! into any server, the second after 72 hours of use ([`UsageClock`]), both only at a calm moment
//! ([`CalmGate`]); after that, never again. No network involved.

use std::time::{Duration, Instant};

/// A Pix: what the BR Code says. Only ASCII reaches the code (see [`field_text`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixInfo {
    /// The Pix key as registered: e-mail, random key, CPF/CNPJ (digits) or phone (`+55...`). At
    /// most [`KEY_MAX`] characters.
    pub key: &'static str,
    /// The receiver's name; the code keeps its first [`NAME_MAX`] letters.
    pub name: &'static str,
    /// The receiver's city; the code keeps its first [`CITY_MAX`] letters.
    pub city: &'static str,
}

/// The project's Pix: the one place to change (e.g.
/// `Some(PixInfo { key: "...", name: "...", city: "..." })`). `None` is a safe release: see above.
pub const PIX: Option<PixInfo> = Some(PixInfo { key: "4957c225-731a-4c88-bc63-40eeb5417b47", name: "cezar callado", city: "Sao Paulo" });

/// A key that is obviously no key, to exercise the interface. Never in a normal build, tests apart.
#[cfg(any(test, feature = "donate-test"))]
pub const TEST_PIX: PixInfo = PixInfo { key: "test-key-do-not-ship", name: "Teste do concord", city: "São Paulo" };

#[cfg(feature = "donate-test")]
static USE_TEST_PIX: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The `donate` autotest: [`pix`] answers [`TEST_PIX`] from now on.
#[cfg(feature = "donate-test")]
pub fn use_test_pix() {
    USE_TEST_PIX.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// The Pix in force: [`PIX`].
pub fn pix() -> Option<PixInfo> {
    #[cfg(feature = "donate-test")]
    if USE_TEST_PIX.load(std::sync::atomic::Ordering::Relaxed) {
        return Some(TEST_PIX);
    }
    PIX
}

// ---------------------------------------------------------------- the BR Code

/// The longest key that fits field 26 (99 characters, 18 of them the Pix identifier and 4 the
/// key's own header).
#[cfg(test)]
pub const KEY_MAX: usize = 77;
pub const NAME_MAX: usize = 25;
pub const CITY_MAX: usize = 15;

/// One EMV field: id, two-digit length, value. (`value` is ASCII, so bytes are characters.)
fn field(id: &str, value: &str) -> String {
    format!("{id}{:02}{value}", value.len())
}

/// A name or a city for the code: the accents out (bank apps refuse or garble them), anything else
/// that is not ASCII dropped, at most `max` characters.
fn field_text(text: &str, max: usize) -> String {
    let plain = text.chars().filter_map(|c| match c {
        'á' | 'à' | 'â' | 'ã' | 'ä' => Some('a'),
        'Á' | 'À' | 'Â' | 'Ã' | 'Ä' => Some('A'),
        'é' | 'è' | 'ê' | 'ë' => Some('e'),
        'É' | 'È' | 'Ê' | 'Ë' => Some('E'),
        'í' | 'ì' | 'î' | 'ï' => Some('i'),
        'Í' | 'Ì' | 'Î' | 'Ï' => Some('I'),
        'ó' | 'ò' | 'ô' | 'õ' | 'ö' => Some('o'),
        'Ó' | 'Ò' | 'Ô' | 'Õ' | 'Ö' => Some('O'),
        'ú' | 'ù' | 'û' | 'ü' => Some('u'),
        'Ú' | 'Ù' | 'Û' | 'Ü' => Some('U'),
        'ç' => Some('c'),
        'Ç' => Some('C'),
        'ñ' => Some('n'),
        'Ñ' => Some('N'),
        c if c.is_ascii() && !c.is_ascii_control() => Some(c),
        _ => None,
    });
    plain.take(max).collect::<String>().trim_end().to_string()
}

/// CRC-16/CCITT-FALSE (polynomial 0x1021, initial value 0xFFFF), the BR Code's field 63.
fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for b in data {
        crc ^= u16::from(*b) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x1021 } else { crc << 1 };
        }
    }
    crc
}

/// The static BR Code of `pix`, with no amount (whoever pays chooses it): the "Pix copia e cola"
/// text, and what the QR code says.
pub fn br_code(pix: &PixInfo) -> String {
    let account = field("00", "br.gov.bcb.pix") + &field("01", pix.key);
    let mut code = field("00", "01");
    code += &field("26", &account);
    code += &field("52", "0000"); // Merchant category: none.
    code += &field("53", "986"); // Currency: BRL.
    code += &field("58", "BR");
    code += &field("59", &field_text(pix.name, NAME_MAX));
    code += &field("60", &field_text(pix.city, CITY_MAX));
    code += &field("62", &field("05", "***")); // Transaction id: none (a static code).
    code += "6304";
    let crc = crc16(code.as_bytes());
    code + &format!("{crc:04X}")
}

/// The QR code of `text`: its modules row by row (true: dark) and the side, with no quiet zone.
pub fn qr_modules(text: &str) -> Option<(usize, Vec<bool>)> {
    let code = qrcode::QrCode::with_error_correction_level(text, qrcode::EcLevel::M).ok()?;
    let side = code.width();
    let modules = code.to_colors().into_iter().map(|c| c == qrcode::Color::Dark).collect();
    Some((side, modules))
}

// ---------------------------------------------------------------- the two popups

/// At most this many popups, ever.
pub const MAX_POPUPS: u32 = 2;
/// The second popup: this much use, accumulated.
pub const SECOND_POPUP_SECS: u64 = 72 * 3600;
/// A popup waits for this long a calm stretch (nothing opens a dialog of its own just after the
/// login: the owner's password prompt, say, comes right behind it).
pub const CALM_SECS: f32 = 3.0;

/// A popup is owed: the first one as soon as anyone is logged in, the second once 72 hours of use
/// are in. (`shown` counts what the user already got: dismissed or not.)
pub fn popup_due(shown: u32, usage_secs: u64, logged_in: bool) -> bool {
    logged_in
        && match shown {
            0 => true,
            1 => usage_secs >= SECOND_POPUP_SECS,
            _ => false,
        }
}

/// A popup went up: count it. A first popup that had to wait through 72 hours of calls does not
/// bring the second right behind it: the 72 hours start again.
pub fn mark_shown(shown: &mut u32, usage_secs: &mut u64) {
    if *shown == 0 && *usage_secs >= SECOND_POPUP_SECS {
        *usage_secs = 0;
    }
    *shown = (*shown + 1).min(MAX_POPUPS);
}

/// "Wait for the next calm moment": true once it has been calm for `CALM_SECS` in a row.
#[derive(Debug, Default)]
pub struct CalmGate {
    since: Option<Instant>,
}

impl CalmGate {
    pub fn ready(&mut self, calm: bool, now: Instant) -> bool {
        if !calm {
            self.since = None;
            return false;
        }
        let since = *self.since.get_or_insert(now);
        now.duration_since(since).as_secs_f32() >= CALM_SECS
    }
}

/// How often the usage counter looks at the clock (one wake-up, tray included).
const CLOCK_PERIOD: Duration = Duration::from_secs(120);
/// A gap longer than this many periods is not use: the PC was asleep.
const MAX_GAP_PERIODS: u32 = 3;
/// settings.json gets the count at most this often (and when counting stops).
const SAVE_EVERY_SECS: u64 = 300;

/// What one look at the clock gave.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Counted {
    /// Seconds of use to add.
    pub secs: u64,
    pub save: bool,
}

/// Counts the time logged in to a server: from `logic()`, which also runs in the tray, with one
/// timed wake-up each [`CLOCK_PERIOD`]. Minutes-level accuracy; a suspended PC does not count.
#[derive(Debug)]
pub struct UsageClock {
    period: Duration,
    last: Option<Instant>,
    unsaved: u64,
}

impl Default for UsageClock {
    fn default() -> Self {
        Self::with_period(CLOCK_PERIOD)
    }
}

impl UsageClock {
    pub fn with_period(period: Duration) -> Self {
        UsageClock { period, last: None, unsaved: 0 }
    }

    fn since(&self, last: Instant, now: Instant) -> u64 {
        let gap = now.duration_since(last);
        if gap > self.period * MAX_GAP_PERIODS {
            0
        } else {
            gap.as_secs()
        }
    }

    /// One look (cheap: a comparison, most of the time). `counting`: logged in to a server.
    pub fn tick(&mut self, now: Instant, counting: bool) -> Counted {
        let Some(last) = self.last else {
            self.last = counting.then_some(now);
            return Counted::default();
        };
        if counting && now.duration_since(last) < self.period {
            return Counted::default();
        }
        let secs = self.since(last, now);
        self.last = counting.then_some(now);
        self.unsaved += secs;
        let save = self.unsaved > 0 && (!counting || self.unsaved >= SAVE_EVERY_SECS);
        if save {
            self.unsaved = 0;
        }
        Counted { secs, save }
    }

    /// When the next look is due (nothing while not counting).
    pub fn next(&self, now: Instant) -> Option<Duration> {
        self.last.map(|last| (last + self.period).saturating_duration_since(now))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// The Banco Central's example of a static BR Code (Manual de Padrões para Iniciação do Pix).
    const BCB_EXAMPLE: &str = "00020126580014br.gov.bcb.pix0136123e4567-e12b-12d1-a456-4266554400005204000053039865802BR5913Fulano de Tal6008BRASILIA62070503***63041D3D";

    /// The fields of an EMV text, as a phone's reader splits them.
    fn decode(text: &str) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        let mut rest = text;
        while !rest.is_empty() {
            let (id, tail) = rest.split_at(2);
            let (len, tail) = tail.split_at(2);
            let (value, tail) = tail.split_at(len.parse::<usize>().expect("a two-digit length"));
            assert!(out.insert(id.to_string(), value.to_string()).is_none(), "field {id} twice");
            rest = tail;
        }
        out
    }

    fn check_crc(code: &str) {
        let (body, crc) = code.split_at(code.len() - 4);
        assert!(body.ends_with("6304"));
        assert_eq!(crc, format!("{:04X}", crc16(body.as_bytes())));
    }

    #[test]
    fn crc_is_ccitt_false() {
        // The catalogue's check value of CRC-16/CCITT-FALSE.
        assert_eq!(crc16(b"123456789"), 0x29B1);
        check_crc(BCB_EXAMPLE);
    }

    #[test]
    fn the_banco_central_example_comes_out_identical() {
        let pix = PixInfo { key: "123e4567-e12b-12d1-a456-426655440000", name: "Fulano de Tal", city: "BRASILIA" };
        assert_eq!(br_code(&pix), BCB_EXAMPLE);
    }

    #[test]
    fn the_payload_decodes_back_to_its_fields() {
        let code = br_code(&TEST_PIX);
        check_crc(&code);
        let f = decode(&code);
        assert_eq!(f["00"], "01");
        assert_eq!(f["52"], "0000");
        assert_eq!(f["53"], "986");
        assert_eq!(f["58"], "BR");
        assert_eq!(f["59"], "Teste do concord");
        assert_eq!(f["60"], "Sao Paulo");
        let account = decode(&f["26"]);
        assert_eq!(account["00"], "br.gov.bcb.pix");
        assert_eq!(account["01"], TEST_PIX.key);
        assert_eq!(decode(&f["62"])["05"], "***");
        assert_eq!(f["63"].len(), 4);
    }

    #[test]
    fn name_and_city_are_plain_and_within_their_limits() {
        let pix = PixInfo { key: "k", name: "João Conceição Araújo de Magalhães Nunes", city: "São José do Rio Preto" };
        let f = decode(&br_code(&pix));
        assert_eq!(f["59"], "Joao Conceicao Araujo de");
        assert_eq!(f["60"], "Sao Jose do Rio");
        assert!(f["59"].len() <= NAME_MAX && f["60"].len() <= CITY_MAX);
        assert_eq!(field_text("Ação 🎉", 25), "Acao");
    }

    #[test]
    fn the_qr_code_is_a_square_with_its_finder_patterns() {
        let (side, m) = qr_modules(&br_code(&TEST_PIX)).expect("it fits a QR code");
        assert_eq!(m.len(), side * side);
        assert_eq!((side - 17) % 4, 0, "a QR side is 17 + 4 × version");
        let dark = |x: usize, y: usize| m[y * side + x];
        // The three corners' finder patterns (a dark ring, a light ring, a dark centre).
        for (x, y) in [(0, 0), (side - 7, 0), (0, side - 7)] {
            assert!(dark(x, y) && dark(x + 6, y + 6) && !dark(x + 1, y + 1) && dark(x + 3, y + 3));
        }
    }

    /// The Pix in the source (none, or the user's) can be shipped as it is.
    #[test]
    fn the_configured_pix_is_valid_and_not_the_test_one() {
        let Some(pix) = PIX else {
            assert_eq!(pix(), None);
            return;
        };
        assert_ne!(pix.key, TEST_PIX.key);
        assert!(!pix.key.is_empty() && pix.key.len() <= KEY_MAX && pix.key.is_ascii());
        assert!(!field_text(pix.name, NAME_MAX).is_empty() && !field_text(pix.city, CITY_MAX).is_empty());
        check_crc(&br_code(&pix));
        assert!(qr_modules(&br_code(&pix)).is_some());
    }

    /// Not a check: writes the real Pix's QR code to `dist/pix-qr-teste.png`, to try with a bank's
    /// app: `cargo test -p voxhub-app pix_qr_png -- --ignored`.
    #[test]
    #[ignore = "writes a file"]
    fn pix_qr_png() {
        const QUIET: usize = 4;
        const SCALE: usize = 10;
        let (side, m) = qr_modules(&br_code(&PIX.expect("a Pix is set"))).expect("it fits a QR code");
        let full = ((side + 2 * QUIET) * SCALE) as u32;
        let picture = image::GrayImage::from_fn(full, full, |x, y| {
            let (x, y) = (x as usize / SCALE, y as usize / SCALE);
            let inside = (QUIET..side + QUIET).contains(&x) && (QUIET..side + QUIET).contains(&y);
            image::Luma([if inside && m[(y - QUIET) * side + x - QUIET] { 0 } else { 255 }])
        });
        picture.save(concat!(env!("CARGO_MANIFEST_DIR"), "/../dist/pix-qr-teste.png")).expect("the file");
    }

    /// What the app does, in small: `show` is one calm moment on a server.
    fn show(shown: &mut u32, usage: &mut u64) -> bool {
        let due = popup_due(*shown, *usage, true);
        if due {
            mark_shown(shown, usage);
        }
        due
    }

    #[test]
    fn two_popups_ever() {
        let (mut shown, mut usage) = (0, 0);
        assert!(!popup_due(shown, usage, false), "not before a login");
        assert!(show(&mut shown, &mut usage), "first login");
        assert_eq!(shown, 1);
        assert!(!show(&mut shown, &mut usage), "a second server");
        usage = SECOND_POPUP_SECS - 1;
        assert!(!show(&mut shown, &mut usage), "just short of 72 hours");
        usage = SECOND_POPUP_SECS;
        assert!(!popup_due(shown, usage, false), "72 hours in, on the home screen");
        assert!(show(&mut shown, &mut usage), "72 hours");
        assert_eq!(shown, 2);
        usage = 100 * SECOND_POPUP_SECS;
        assert!(!show(&mut shown, &mut usage), "never a third");
        // A strange file: still nothing, and the count never grows past the limit.
        assert!(!popup_due(7, u64::MAX, true));
        mark_shown(&mut shown, &mut usage);
        assert_eq!(shown, MAX_POPUPS);
    }

    #[test]
    fn a_late_first_popup_does_not_bring_the_second_behind_it() {
        let (mut shown, mut usage) = (0, SECOND_POPUP_SECS + 5);
        assert!(show(&mut shown, &mut usage));
        assert!(!show(&mut shown, &mut usage));
    }

    #[test]
    fn the_popup_waits_for_a_calm_stretch() {
        let t0 = Instant::now();
        let after = |s: f32| t0 + Duration::from_secs_f32(s);
        let mut gate = CalmGate::default();
        assert!(!gate.ready(false, t0), "in a call");
        assert!(!gate.ready(false, after(60.0)), "still in the call");
        assert!(!gate.ready(true, after(61.0)), "just out of it");
        assert!(!gate.ready(true, after(61.0 + CALM_SECS / 2.0)));
        assert!(!gate.ready(false, after(62.0 + CALM_SECS / 2.0)), "a dialog opened");
        assert!(!gate.ready(true, after(63.0)), "calm again: the wait starts over");
        assert!(gate.ready(true, after(63.0 + CALM_SECS)));
    }

    #[test]
    fn the_clock_counts_time_logged_in_and_skips_sleep() {
        let t0 = Instant::now();
        let after = |s: u64| t0 + Duration::from_secs(s);
        let mut c = UsageClock::default();
        assert_eq!(c.tick(t0, false), Counted::default(), "the home screen");
        assert_eq!(c.next(t0), None, "no wake-ups while not counting");
        assert_eq!(c.tick(after(10), true), Counted::default(), "logged in: it starts here");
        assert_eq!(c.next(after(10)), Some(CLOCK_PERIOD));
        assert_eq!(c.tick(after(20), true), Counted::default(), "too soon to look");
        assert_eq!(c.tick(after(130), true), Counted { secs: 120, save: false });
        assert_eq!(c.tick(after(250), true), Counted { secs: 120, save: false });
        assert_eq!(c.tick(after(370), true), Counted { secs: 120, save: true }, "five minutes: saved");
        // The PC slept for an hour: that hour is not use.
        assert_eq!(c.tick(after(3970), true), Counted { secs: 0, save: false });
        assert_eq!(c.tick(after(4090), true), Counted { secs: 120, save: false });
        // Leaving keeps the last bit, and what was not saved yet goes to the file.
        assert_eq!(c.tick(after(4120), false), Counted { secs: 30, save: true });
        assert_eq!(c.tick(after(5000), false), Counted::default());
        assert_eq!(c.next(after(5000)), None);
    }
}
