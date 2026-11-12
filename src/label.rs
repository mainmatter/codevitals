//! `cv label`: a rater judges pairs of functions side by side, "Which would you
//! rather have to change?", with three keys: left, right, can't tell.
//!
//! Pairs are production functions at the newest scanned commit, of one language,
//! whose token counts are within 15% of each other, so the shorter one is no tell.
//! About 10% of pairs repeat an earlier pair with sides swapped (the rater's
//! consistency), and about a quarter come from a pool every rater walks in the same
//! order (agreement between raters). The rater is the repository's git `user.email`;
//! a session resumes where the rater stopped, as each choice is sampled from the
//! rater and their count of judgments. The screen shows code and paths, never a
//! metric value.

use std::collections::HashSet;
use std::io::{IsTerminal, Read, Write};
use std::path::Path;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::{cursor, execute, queue, style, terminal};
use duckdb::{Connection, params};

use crate::labels::{self, Function, Label};
use crate::util::Rng;

/// Smallest and largest functions shown, in tokens, and most lines: a pair must be
/// readable at a glance and judged in well under a minute.
const MIN_TOKENS: i64 = 30;
const MAX_TOKENS: i64 = 400;
const MAX_LINES: i64 = 60;
/// Most a pair's token counts may differ, relative to the larger.
const TOKEN_SPREAD: f64 = 0.15;
/// Shares of pairs that repeat an earlier pair, and that come from the shared pool.
const REPEAT_SHARE: f64 = 0.10;
const SHARED_SHARE: f64 = 0.25;

/// A function that may be shown.
#[derive(Clone, Debug)]
pub struct Candidate {
    pub path: String,
    pub lineage: String,
    pub lang: String,
    pub blob: String,
    pub start: i64,
    pub end: i64,
    pub line: i64,
    pub tokens: i64,
}

impl Candidate {
    fn function(&self) -> Function {
        Function {
            blob: self.blob.clone(),
            start: self.start,
            end: self.end,
            path: self.path.clone(),
        }
    }
}

/// A pair to show: indices of the left (a) and right (b) candidates, and the earlier
/// judgment it repeats.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pair {
    pub a: usize,
    pub b: usize,
    pub repeat_of: Option<i64>,
}

/// Where the rater is: the candidates (in a fixed order), the rater's judgments so
/// far, and the seed every rater's shared pool derives from.
pub struct Session {
    pub rater: String,
    pub candidates: Vec<Candidate>,
    pub judged: Vec<Label>,
    pub pool_seed: String,
}

type Key = (String, i64, i64);

fn key(f: &Function) -> Key {
    (f.blob.clone(), f.start, f.end)
}

impl Session {
    /// Unordered pairs the rater has judged.
    fn seen(&self) -> HashSet<(Key, Key)> {
        self.judged
            .iter()
            .map(|l| {
                let (a, b) = (key(&l.a), key(&l.b));
                if a < b { (a, b) } else { (b, a) }
            })
            .collect()
    }

    fn index(&self, f: &Function) -> Option<usize> {
        self.candidates
            .iter()
            .position(|c| (c.blob.as_str(), c.start, c.end) == (f.blob.as_str(), f.start, f.end))
    }

    /// A random pair drawn by `rng`: same language, tokens within TOKEN_SPREAD.
    fn draw(&self, rng: &mut Rng) -> Option<(usize, usize)> {
        for _ in 0..200 {
            let a = rng.below(self.candidates.len());
            let ca = &self.candidates[a];
            let partners: Vec<usize> = (0..self.candidates.len())
                .filter(|&b| {
                    let cb = &self.candidates[b];
                    let (x, y) = (ca.tokens as f64, cb.tokens as f64);
                    b != a && cb.lang == ca.lang && (x - y).abs() <= TOKEN_SPREAD * x.max(y)
                })
                .collect();
            if !partners.is_empty() {
                return Some((a, partners[rng.below(partners.len())]));
            }
        }
        None
    }

    /// The next pair to show.
    pub fn next(&self) -> Result<Pair> {
        let n = self.judged.len();
        let mut rng = Rng::new(&format!("{}\0{n}", self.rater));
        let seen = self.seen();
        let unseen = |a: usize, b: usize| {
            let (x, y) = (
                key(&self.candidates[a].function()),
                key(&self.candidates[b].function()),
            );
            !seen.contains(&if x < y { (x, y) } else { (y, x) })
        };
        let u = rng.unit();
        // A repeat: an earlier, not yet repeated judgment whose functions still exist,
        // sides swapped.
        if u < REPEAT_SHARE {
            let repeated: HashSet<i64> = self.judged.iter().filter_map(|l| l.repeat_of).collect();
            let open: Vec<&Label> = self
                .judged
                .iter()
                .filter(|l| l.repeat_of.is_none() && !repeated.contains(&l.n))
                .filter(|l| self.index(&l.a).is_some() && self.index(&l.b).is_some())
                .collect();
            if !open.is_empty() {
                let l = open[rng.below(open.len())];
                let (a, b) = (
                    self.index(&l.b).expect("checked"),
                    self.index(&l.a).expect("checked"),
                );
                return Ok(Pair {
                    a,
                    b,
                    repeat_of: Some(l.n),
                });
            }
        }
        // The shared pool: the same sequence of pairs for every rater.
        if u < REPEAT_SHARE + SHARED_SHARE {
            for i in 0..10_000 {
                let mut pool = Rng::new(&format!("{}\0{i}", self.pool_seed));
                let Some((a, b)) = self.draw(&mut pool) else {
                    break;
                };
                if unseen(a, b) {
                    return Ok(Pair {
                        a,
                        b,
                        repeat_of: None,
                    });
                }
            }
        }
        for _ in 0..1000 {
            let Some((a, b)) = self.draw(&mut rng) else {
                break;
            };
            if unseen(a, b) {
                return Ok(Pair {
                    a,
                    b,
                    repeat_of: None,
                });
            }
        }
        bail!(
            "no pair of functions of one language within 15% of each other's tokens is left to judge"
        )
    }
}

/// Runs a labelling session: pairs are shown until the rater quits; each judgment is
/// appended to the labels file at once. Returns the number of judgments made.
pub fn run(repo: &gix::Repository, db: &mut Connection, file: &Path) -> Result<usize> {
    let config = repo.config_snapshot();
    let email = config
        .string("user.email")
        .map(|e| e.to_string())
        .filter(|e| !e.is_empty());
    let rater = email
        .context(
            "set your git user.email (`git config user.email ...`): it identifies you as rater",
        )?
        .to_lowercase();
    labels::load(db, file)?;
    let judged = rater_labels(db, &rater)?;
    let (tip, tip_id) = crate::db::tip(db)?;
    crate::metrics::cache_helpers(
        db,
        &["units.functions".into(), "author_file_commits".into()],
    )?;
    let candidates: Vec<Candidate> = db
        .prepare(
            "select path, lineage, lang, blob, start_byte, end_byte, line, tokens from units.functions
             where since <= $1 and $1 < until and tokens between $2 and $3 and end_line - line < $4
             order by path, start_byte",
        )?
        .query_map(params![tip, MIN_TOKENS, MAX_TOKENS, MAX_LINES], |r| {
            Ok(Candidate {
                path: r.get(0)?,
                lineage: r.get(1)?,
                lang: r.get(2)?,
                blob: r.get(3)?,
                start: r.get(4)?,
                end: r.get(5)?,
                line: r.get(6)?,
                tokens: r.get(7)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    ensure!(
        candidates.len() >= 2,
        "fewer than two production functions of {MIN_TOKENS} to {MAX_TOKENS} tokens to compare"
    );
    let mut session = Session {
        rater: rater.clone(),
        candidates,
        judged,
        pool_seed: tip_id,
    };
    let commits = |db: &Connection, lineage: &str| -> Result<i64> {
        Ok(db.query_row(
            "select coalesce(sum(commits), 0) from author_file_commits where identity = $1 and lineage = $2",
            params![rater, lineage],
            |r| r.get(0),
        )?)
    };
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let mut screen = Screen::new(interactive)?;
    let mut made = 0;
    let result = (|| -> Result<()> {
        loop {
            let pair = session.next()?;
            let (a, b) = (&session.candidates[pair.a], &session.candidates[pair.b]);
            let text = |c: &Candidate| -> Result<String> {
                let data = repo
                    .find_blob(crate::db::oid(&c.blob))
                    .with_context(|| {
                        format!(
                            "read {} (blob {}) from {:?}",
                            c.path,
                            c.blob,
                            repo.git_dir()
                        )
                    })?
                    .take_data();
                Ok(String::from_utf8_lossy(&data[c.start as usize..c.end as usize]).into_owned())
            };
            let shown = Instant::now();
            let (a_text, b_text) = (text(a)?, text(b)?);
            let choice = match screen.ask([(a, &a_text), (b, &b_text)], session.judged.len())? {
                Answer::Choice(choice) => choice,
                Answer::Quit => return Ok(()),
            };
            let label = Label {
                rater: rater.clone(),
                n: session.judged.len() as i64,
                time: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64,
                a: a.function(),
                b: b.function(),
                choice,
                ms: shown.elapsed().as_millis() as i64,
                a_commits: commits(db, &a.lineage)?,
                b_commits: commits(db, &b.lineage)?,
                repeat_of: pair.repeat_of,
            };
            labels::append(file, &label)?;
            session.judged.push(label);
            made += 1;
        }
    })();
    screen.close()?;
    result.map(|()| made)
}

/// The rater's judgments in the order made.
fn rater_labels(db: &Connection, rater: &str) -> Result<Vec<Label>> {
    let rows = db
        .prepare(
            "select n, epoch(time)::bigint, a_blob, a_start, a_end, a_path, b_blob, b_start, b_end, b_path,
                    choice, ms, a_commits, b_commits, repeat_of
             from labels where rater = $1 order by n",
        )?
        .query_map(params![rater], |r| {
            let f = |i: usize| -> duckdb::Result<Function> {
                Ok(Function { blob: r.get(i)?, start: r.get(i + 1)?, end: r.get(i + 2)?, path: r.get(i + 3)? })
            };
            let choice: String = r.get(10)?;
            Ok(Label {
                rater: rater.to_owned(),
                n: r.get(0)?,
                time: r.get(1)?,
                a: f(2)?,
                b: f(6)?,
                choice: match choice.as_str() {
                    "a" => "a",
                    "b" => "b",
                    _ => "same",
                },
                ms: r.get(11)?,
                a_commits: r.get(12)?,
                b_commits: r.get(13)?,
                repeat_of: r.get(14)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    for (i, l) in rows.iter().enumerate() {
        ensure!(
            l.n == i as i64,
            "the labels of {rater} are not numbered 0, 1, 2, ...: merge files without renumbering"
        );
    }
    Ok(rows)
}

/// The terminal: an alternate screen in raw mode, or, when input is not a terminal,
/// keys read from stdin (h, l, s, q) for scripted sessions.
struct Screen {
    interactive: bool,
}

impl Screen {
    fn new(interactive: bool) -> Result<Screen> {
        if interactive {
            terminal::enable_raw_mode()?;
            execute!(
                std::io::stdout(),
                terminal::EnterAlternateScreen,
                cursor::Hide
            )?;
        }
        Ok(Screen { interactive })
    }

    fn close(&mut self) -> Result<()> {
        if self.interactive {
            execute!(
                std::io::stdout(),
                cursor::Show,
                terminal::LeaveAlternateScreen
            )?;
            terminal::disable_raw_mode()?;
            self.interactive = false;
        }
        Ok(())
    }

    /// Shows the pair and returns the rater's answer.
    fn ask(&mut self, pair: [(&Candidate, &str); 2], n: usize) -> Result<Answer> {
        if !self.interactive {
            let mut byte = [0u8];
            loop {
                if std::io::stdin().read(&mut byte)? == 0 {
                    return Ok(Answer::Quit);
                }
                if let Some(answer) = byte_answer(byte[0]) {
                    return Ok(answer);
                }
            }
        }
        self.draw(pair, n)?;
        loop {
            match crossterm::event::read()? {
                Event::Key(key) => {
                    if let Some(answer) = key_answer(key) {
                        return Ok(answer);
                    }
                }
                Event::Resize(..) => self.draw(pair, n)?,
                _ => {}
            }
        }
    }

    fn draw(&mut self, pair: [(&Candidate, &str); 2], n: usize) -> Result<()> {
        let (width, height) = terminal::size()?;
        let lines = layout(width.into(), height.into(), pair, n);
        let mut out = std::io::stdout();
        queue!(out, terminal::Clear(terminal::ClearType::All))?;
        for (row, line) in lines.iter().enumerate() {
            queue!(out, cursor::MoveTo(0, row as u16), style::Print(line))?;
        }
        out.flush()?;
        Ok(())
    }
}

/// What the rater answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Answer {
    /// `a` (left), `b` (right) or `same` (can't tell).
    Choice(&'static str),
    Quit,
}

/// The answer a key press gives: arrows or h/l/a/b for a side, down, space or s for
/// can't tell, q, Esc or Ctrl-C to quit; other keys, and releases, none.
fn key_answer(key: KeyEvent) -> Option<Answer> {
    if key.kind != KeyEventKind::Press {
        return None;
    }
    Some(match key.code {
        KeyCode::Left | KeyCode::Char('h' | 'a') => Answer::Choice("a"),
        KeyCode::Right | KeyCode::Char('l' | 'b') => Answer::Choice("b"),
        KeyCode::Down | KeyCode::Char(' ' | 's') => Answer::Choice("same"),
        KeyCode::Char('q') | KeyCode::Esc => Answer::Quit,
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Answer::Quit,
        _ => return None,
    })
}

/// The answer a byte of scripted input gives (h, a, l, b, s, q), as `key_answer`.
fn byte_answer(byte: u8) -> Option<Answer> {
    Some(match byte {
        b'h' | b'a' => Answer::Choice("a"),
        b'l' | b'b' => Answer::Choice("b"),
        b's' => Answer::Choice("same"),
        b'q' => Answer::Quit,
        _ => return None,
    })
}

/// The screen for a pair, one string per row, each exactly `width` columns wide:
/// the question, a blank row, both functions side by side (path and line, a blank
/// row, the code, cut to fit), and the keys on the last row. Tabs become four
/// spaces; other control characters (a CRLF file's carriage returns) are dropped, as
/// a terminal would act on them.
fn layout(width: usize, height: usize, pair: [(&Candidate, &str); 2], n: usize) -> Vec<String> {
    let pane = width.saturating_sub(3) / 2;
    let code_rows = height.saturating_sub(3);
    let column = |(c, text): (&Candidate, &str)| -> Vec<String> {
        let mut out = vec![format!("{}:{}", c.path, c.line), String::new()];
        out.extend(text.lines().map(|l| {
            l.replace('\t', "    ")
                .chars()
                .filter(|c| !c.is_control())
                .collect()
        }));
        if out.len() > code_rows {
            out.truncate(code_rows.saturating_sub(1));
            out.push("...".into());
        }
        out
    };
    let (left, right) = (column(pair[0]), column(pair[1]));
    let mut screen = vec![
        fit("Which would you rather have to change?", width),
        fit("", width),
    ];
    for i in 0..code_rows {
        let l = left.get(i).map_or("", String::as_str);
        let r = right.get(i).map_or("", String::as_str);
        screen.push(fit(&format!("{} | {}", fit(l, pane), fit(r, pane)), width));
    }
    let keys = format!("<- left    -> right    down: can't tell    q: quit    ({n} judged)");
    screen.push(fit(&keys, width));
    screen.truncate(height);
    screen
}

/// `text` cut or padded to `width` terminal columns (wide characters take two).
fn fit(text: &str, width: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if used + w > width {
            break;
        }
        out.push(c);
        used += w;
    }
    out.extend(std::iter::repeat_n(' ', width - used));
    out
}

impl Drop for Screen {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidates() -> Vec<Candidate> {
        let c = |i: usize, lang: &str, tokens: i64| Candidate {
            path: format!("f{i}.x"),
            lineage: format!("0:f{i}.x"),
            lang: lang.into(),
            blob: format!("b{i}"),
            start: 0,
            end: 10,
            line: 1,
            tokens,
        };
        (0..200)
            .map(|i| {
                c(
                    i,
                    if i % 2 == 0 { "rust" } else { "python" },
                    50 + 7 * (i % 40) as i64,
                )
            })
            .collect()
    }

    fn judge(s: &mut Session, choice: &'static str) -> Pair {
        let p = s.next().unwrap();
        let (a, b) = (s.candidates[p.a].function(), s.candidates[p.b].function());
        s.judged.push(Label {
            rater: s.rater.clone(),
            n: s.judged.len() as i64,
            time: 0,
            a,
            b,
            choice,
            ms: 1,
            a_commits: 0,
            b_commits: 0,
            repeat_of: p.repeat_of,
        });
        p
    }

    #[test]
    fn pairs_match_language_and_size_and_repeat_swapped() {
        let mut s = Session {
            rater: "r".into(),
            candidates: candidates(),
            judged: vec![],
            pool_seed: "tip".into(),
        };
        let mut repeats = 0;
        for _ in 0..200 {
            let p = judge(&mut s, "a");
            let (a, b) = (&s.candidates[p.a], &s.candidates[p.b]);
            assert_eq!(a.lang, b.lang);
            let (x, y) = (a.tokens as f64, b.tokens as f64);
            assert!((x - y).abs() <= 0.15 * x.max(y), "{x} vs {y}");
            if p.repeat_of.is_some() {
                repeats += 1;
            }
        }
        // Repeats swap sides: the earlier left is now right.
        for l in s.judged.iter().filter(|l| l.repeat_of.is_some()) {
            let e = &s.judged[l.repeat_of.unwrap() as usize];
            assert_eq!((&l.a, &l.b), (&e.b, &e.a));
        }
        assert!((10..=35).contains(&repeats), "{repeats} repeats of 200");
    }

    #[test]
    fn keys_and_bytes_answer_alike() {
        use crossterm::event::KeyEventState;
        let key = |code, kind| KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind,
            state: KeyEventState::NONE,
        };
        let press = |code| key_answer(key(code, KeyEventKind::Press));
        assert_eq!(press(KeyCode::Left), Some(Answer::Choice("a")));
        assert_eq!(press(KeyCode::Right), Some(Answer::Choice("b")));
        assert_eq!(press(KeyCode::Down), Some(Answer::Choice("same")));
        assert_eq!(press(KeyCode::Esc), Some(Answer::Quit));
        assert_eq!(press(KeyCode::Up), None);
        assert_eq!(key_answer(key(KeyCode::Left, KeyEventKind::Release)), None);
        let ctrl_c = KeyEvent {
            modifiers: KeyModifiers::CONTROL,
            ..key(KeyCode::Char('c'), KeyEventKind::Press)
        };
        assert_eq!(key_answer(ctrl_c), Some(Answer::Quit));
        for b in *b"halbsqx\n" {
            assert_eq!(
                byte_answer(b),
                press(KeyCode::Char(b as char)),
                "{}",
                b as char
            );
        }
    }

    #[test]
    fn layout_fits_any_terminal() {
        let c = |path: &str| Candidate {
            path: path.into(),
            lineage: String::new(),
            lang: "rust".into(),
            blob: String::new(),
            start: 0,
            end: 0,
            line: 7,
            tokens: 50,
        };
        let (a, b) = (c("src/a.rs"), c("src/日本.rs"));
        let long: String = (0..100)
            .map(|i| format!("    let x{i} = \"界\t{i}\";\r\n"))
            .collect();
        use unicode_width::UnicodeWidthStr;
        for (w, h) in [(120, 40), (80, 24), (20, 6), (3, 3), (1, 1), (0, 0)] {
            let screen = layout(w, h, [(&a, "fn a() {}\r\n"), (&b, &long)], 3);
            assert!(screen.len() <= h, "{w}x{h}: {} rows", screen.len());
            for row in &screen {
                assert_eq!(row.width(), w, "{w}x{h}: {row:?}");
                assert!(!row.chars().any(char::is_control), "{row:?}");
            }
        }
        let screen = layout(80, 10, [(&a, "fn a() {}\n"), (&b, &long)], 3);
        assert!(screen[0].starts_with("Which would you rather have to change?"));
        assert!(
            screen[2].starts_with("src/a.rs:7") && screen[2].contains("| src/日本.rs:7"),
            "{:?}",
            screen[2]
        );
        assert!(screen[8].contains("| ..."), "{:?}", screen[8]);
        assert!(screen[9].starts_with("<- left") && screen[9].contains("(3 judged)"));
    }

    #[test]
    fn raters_share_pool_pairs_and_resume() {
        let session = |rater: &str| Session {
            rater: rater.into(),
            candidates: candidates(),
            judged: vec![],
            pool_seed: "tip".into(),
        };
        let (mut a, mut b) = (session("ann"), session("bob"));
        let pairs = |s: &Session| -> HashSet<(Key, Key)> { s.seen() };
        for _ in 0..40 {
            judge(&mut a, "a");
            judge(&mut b, "b");
        }
        let shared = pairs(&a).intersection(&pairs(&b)).count();
        assert!(shared >= 5, "{shared} shared pairs");
        // Resuming: the next pair depends only on the rater and what they judged.
        let mut resumed = session("ann");
        resumed.judged = a.judged.clone();
        assert_eq!(resumed.next().unwrap(), a.next().unwrap());
    }
}
