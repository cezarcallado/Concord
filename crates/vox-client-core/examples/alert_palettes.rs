//! Renders every alert of every [`Palette`] to 48 kHz 16-bit stereo WAV files plus a pt-BR
//! listening page (`ouvir.html`), and prints a numeric table per palette. It never plays anything.
//! The output folder is the one given (default `dist/sons-notificacao`, relative to the current
//! directory); the page links the WAVs relatively, so the folder can be moved or shared.
//!
//! ```text
//! cargo run -p vox-client-core --example alert_palettes -- dist/sons-notificacao
//! ```

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use vox_client_core::audio::alerts::{perceived_level_db, AlertKind, Palette};
use vox_client_core::audio::{MIX_CHANNELS, MIX_RATE};

/// Folder name of a palette (no spaces, so the page's relative links stay simple).
fn dir_name(p: Palette) -> &'static str {
    match p {
        Palette::Atual => "Atual",
        Palette::Madeira => "Madeira",
        Palette::Gota => "Gota",
        Palette::SinoSuave => "Sino-suave",
    }
}

fn blurb(p: Palette) -> &'static str {
    match p {
        Palette::Atual => "os sons até a versão 1.0: agudos e curtos",
        Palette::Madeira => "marimba / madeira, notas de Sol a Sol",
        Palette::Gota => "bolhas redondas que \"sobem\" na nota",
        Palette::SinoSuave => "o padrão: sino grave e macio, a família clássica uma oitava abaixo",
    }
}

/// pt-BR row label of an alert.
fn label(k: AlertKind) -> &'static str {
    match k {
        AlertKind::Join => "Alguém entrou na sala",
        AlertKind::Leave => "Alguém saiu da sala",
        AlertKind::Online => "Alguém ficou online",
        AlertKind::Offline => "Alguém ficou offline",
        AlertKind::Mute => "Microfone desligado",
        AlertKind::Unmute => "Microfone ligado",
        AlertKind::Deafen => "Áudio desligado (ensurdecer)",
        AlertKind::Undeafen => "Áudio ligado de novo",
        AlertKind::StreamStart => "Alguém começou uma transmissão",
        AlertKind::StreamStop => "Alguém encerrou a transmissão",
        AlertKind::Viewer => "Alguém começou a assistir a sua transmissão",
        AlertKind::DirectMessage => "Mensagem direta",
        AlertKind::Ring => "Chamada tocando (repete enquanto chama)",
        AlertKind::ChannelMessage => "Mensagem no canal",
        AlertKind::Mention => "Menção",
    }
}

fn file_name(k: AlertKind) -> String {
    let nn = AlertKind::ALL.iter().position(|x| *x == k).unwrap() + 1;
    format!("{nn:02}-{}.wav", k.name())
}

/// The "Tocar sequência" demo: join, channel message, mention, mute, unmute, leave.
const SEQUENCE: [AlertKind; 6] = [AlertKind::Join, AlertKind::ChannelMessage, AlertKind::Mention, AlertKind::Mute, AlertKind::Unmute, AlertKind::Leave];

fn write_wav(path: &Path, samples: &[i16]) -> std::io::Result<()> {
    let channels = MIX_CHANNELS as u16;
    let data_len = (samples.len() * 2) as u32;
    let mut b = Vec::with_capacity(44 + data_len as usize);
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data_len).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes()); // PCM
    b.extend_from_slice(&channels.to_le_bytes());
    b.extend_from_slice(&MIX_RATE.to_le_bytes());
    b.extend_from_slice(&(MIX_RATE * channels as u32 * 2).to_le_bytes());
    b.extend_from_slice(&(channels * 2).to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        b.extend_from_slice(&s.to_le_bytes());
    }
    std::fs::write(path, b)
}

/// In-place radix-2 FFT (`re.len()` a power of two).
fn fft(re: &mut [f64], im: &mut [f64]) {
    let n = re.len();
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let ang = -std::f64::consts::TAU / len as f64;
        for start in (0..n).step_by(len) {
            for k in 0..len / 2 {
                let (wr, wi) = ((ang * k as f64).cos(), (ang * k as f64).sin());
                let (a, b) = (start + k, start + k + len / 2);
                let (xr, xi) = (re[b] * wr - im[b] * wi, re[b] * wi + im[b] * wr);
                re[b] = re[a] - xr;
                im[b] = im[a] - xi;
                re[a] += xr;
                im[a] += xi;
            }
        }
        len <<= 1;
    }
}

struct Stats {
    ms: f32,
    peak_db: f32,
    rms_db: f32,
    level_db: f32,
    centroid_hz: f32,
}

fn stats(stereo: &[i16]) -> Stats {
    let mono: Vec<f64> = stereo.iter().step_by(MIX_CHANNELS).map(|s| *s as f64 / i16::MAX as f64).collect();
    let n = mono.len();
    let peak = mono.iter().fold(0f64, |m, s| m.max(s.abs()));
    let rms = (mono.iter().map(|s| s * s).sum::<f64>() / n as f64).sqrt();
    let size = n.next_power_of_two();
    let (mut re, mut im) = (vec![0f64; size], vec![0f64; size]);
    re[..n].copy_from_slice(&mono);
    fft(&mut re, &mut im);
    let bin_hz = MIX_RATE as f64 / size as f64;
    let (mut num, mut den) = (0f64, 0f64);
    for i in (20.0 / bin_hz).ceil() as usize..size / 2 {
        let mag = (re[i] * re[i] + im[i] * im[i]).sqrt();
        num += mag * i as f64 * bin_hz;
        den += mag;
    }
    Stats {
        ms: n as f32 * 1000.0 / MIX_RATE as f32,
        peak_db: 20.0 * peak.log10() as f32,
        rms_db: 20.0 * rms.log10() as f32,
        level_db: perceived_level_db(stereo),
        centroid_hz: (num / den) as f32,
    }
}

fn html() -> String {
    let mut h = String::new();
    h.push_str(
        r#"<!doctype html>
<html lang="pt-BR">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Sons de notificação</title>
<style>
  :root { color-scheme: dark; --bg: #16171a; --panel: #1f2125; --line: #2c2f35; --text: #e6e7ea; --dim: #9a9ea8; --accent: #6d8cff; }
  * { box-sizing: border-box; }
  body { margin: 0; padding: 24px 16px 48px; background: var(--bg); color: var(--text); font: 15px/1.45 system-ui, "Segoe UI", sans-serif; }
  main { max-width: 1100px; margin: 0 auto; }
  h1 { font-size: 22px; margin: 0 0 4px; font-weight: 600; }
  p.sub { color: var(--dim); margin: 0 0 20px; }
  .wrap { overflow-x: auto; border: 1px solid var(--line); border-radius: 10px; }
  table { border-collapse: collapse; width: 100%; min-width: 720px; }
  th, td { padding: 9px 12px; border-bottom: 1px solid var(--line); text-align: center; }
  th:first-child, td:first-child { text-align: left; }
  thead th { background: var(--panel); vertical-align: top; font-weight: 600; }
  thead small { display: block; color: var(--dim); font-weight: 400; font-size: 12px; margin: 2px 0 8px; }
  tbody tr:hover { background: #1b1d21; }
  tbody tr:last-child td { border-bottom: 0; }
  button { background: var(--panel); color: var(--text); border: 1px solid var(--line); border-radius: 8px; padding: 6px 12px; font: inherit; cursor: pointer; }
  button:hover { border-color: var(--accent); }
  button.playing { background: var(--accent); border-color: var(--accent); color: #fff; }
  button.seq { font-size: 13px; }
  .vol { display: flex; gap: 10px; align-items: center; color: var(--dim); margin: 0 0 14px; }
</style>
</head>
<body>
<main>
<h1>Sons de notificação: as paletas</h1>
<p class="sub">Cada coluna é uma paleta; cada linha, um aviso do app. "Sino suave" é o padrão; "Clássico (1.0)" são os sons até a versão 1.0. No app, a escolha fica em Configurações → Notificações. "Tocar sequência" toca: entrou na sala, mensagem no canal, menção, microfone desligado, microfone ligado, saiu da sala.</p>
<label class="vol">Volume <input id="vol" type="range" min="0" max="1" step="0.05" value="0.8"></label>
<div class="wrap">
<table>
<thead>
<tr><th>Aviso</th>"#,
    );
    for p in Palette::ALL {
        let seq: Vec<String> = SEQUENCE.iter().map(|k| format!("{}/{}", dir_name(p), file_name(*k))).collect();
        let _ = write!(
            h,
            "<th>{}<small>{}</small><button class=\"seq\" data-seq=\"{}\">Tocar sequência</button></th>",
            p.label(),
            blurb(p),
            seq.join(" ")
        );
    }
    h.push_str("</tr>\n</thead>\n<tbody>\n");
    for k in AlertKind::ALL {
        let _ = write!(h, "<tr><td>{}</td>", label(k));
        for p in Palette::ALL {
            let src = format!("{}/{}", dir_name(p), file_name(k));
            let _ = write!(h, "<td><button class=\"one\" data-src=\"{src}\" title=\"{}: {}\">&#9654; Tocar</button><audio preload=\"auto\" src=\"{src}\"></audio></td>", p.label(), label(k));
        }
        h.push_str("</tr>\n");
    }
    h.push_str(
        r#"</tbody>
</table>
</div>
</main>
<script>
  const vol = document.getElementById('vol');
  const cache = new Map();
  function audio(src) {
    if (!cache.has(src)) {
      const el = document.querySelector('audio[src="' + src + '"]') || new Audio(src);
      cache.set(src, el);
    }
    return cache.get(src);
  }
  let timers = [];
  function stopAll() {
    timers.forEach(clearTimeout);
    timers = [];
    cache.forEach(a => { a.pause(); a.currentTime = 0; });
    document.querySelectorAll('button.playing').forEach(b => b.classList.remove('playing'));
  }
  function play(src, btn) {
    const a = audio(src);
    a.volume = Number(vol.value);
    a.currentTime = 0;
    a.play();
    if (btn) {
      btn.classList.add('playing');
      a.onended = () => btn.classList.remove('playing');
    }
  }
  document.querySelectorAll('button.one').forEach(b => b.addEventListener('click', () => { stopAll(); play(b.dataset.src, b); }));
  document.querySelectorAll('button.seq').forEach(b => b.addEventListener('click', () => {
    stopAll();
    b.classList.add('playing');
    const list = b.dataset.seq.split(' ');
    list.forEach((src, i) => timers.push(setTimeout(() => play(src), i * 900)));
    timers.push(setTimeout(() => b.classList.remove('playing'), list.length * 900));
  }));
</script>
</body>
</html>
"#,
    );
    h
}

fn main() -> std::io::Result<()> {
    let out = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| "dist/sons-notificacao".into()));
    for p in Palette::ALL {
        let dir = out.join(dir_name(p));
        std::fs::create_dir_all(&dir)?;
        println!("\n## {}\n", p.label());
        println!("| # | alerta | centroide Hz | pico dBFS | RMS dBFS | nível B dB | ms |");
        println!("|---|---|---:|---:|---:|---:|---:|");
        let mut sums = [0f32; 5];
        for (i, k) in AlertKind::ALL.iter().enumerate() {
            let samples = k.samples_in(p);
            write_wav(&dir.join(file_name(*k)), &samples)?;
            let s = stats(&samples);
            println!("| {:02} | {} | {:.0} | {:.1} | {:.1} | {:.1} | {:.0} |", i + 1, k.name(), s.centroid_hz, s.peak_db, s.rms_db, s.level_db, s.ms);
            for (acc, v) in sums.iter_mut().zip([s.centroid_hz, s.peak_db, s.rms_db, s.level_db, s.ms]) {
                *acc += v;
            }
        }
        let n = AlertKind::ALL.len() as f32;
        println!("| | **média** | {:.0} | {:.1} | {:.1} | {:.1} | {:.0} |", sums[0] / n, sums[1] / n, sums[2] / n, sums[3] / n, sums[4] / n);
    }
    let page = out.join("ouvir.html");
    std::fs::write(&page, html())?;
    println!("\nwrote {}", page.display());
    Ok(())
}
