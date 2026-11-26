// SPDX-License-Identifier: Apache-2.0

//! v0.6.19 (F1): the gate of the English output. The theme of the phase is
//! one — everything a user or a client reads is in English — and this is
//! its definition of done. ONE session walks every surface that prints
//! words: the human output of each subcommand, the errors with their exit
//! codes, the prose of `--json`, the review page, the MCP server (stdio
//! and HTTP), the OAuth pages. Nothing is translated here: the test is RED
//! on `a5a61f2` by design and turns green when F2 is done.
//!
//! What counts as Italian is a list of markers, each a word (or a root) the
//! English language does not have. The list is minimal, and it covers every
//! prose row of the inventory of F1 but three: 103 of its markers are the
//! only ones standing on at least one row, so a leftover string cannot
//! slip because nobody thought of its words. The data of the session is
//! English, so nothing has to be excluded.
//!
//! It is a list, not a language detector: on strings this short ("coda
//! vuota", "rifiutata") a detector is a lottery, and a false green is the
//! one failure a gate must not have.

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use engramma_core::cell::proposals::{ProposalInput, ProposalMode, ProposeContext};
use engramma_core::format::{At, Lang, Note, NoteType};
use engramma_core::{Cell, Confidence, NotePatch, Status};

const BIN: &str = env!("CARGO_BIN_EXE_engramma");
const DEADLINE: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// the list: Italian words the English language does not have
// ---------------------------------------------------------------------------

/// Whole words. The number is how many rows of the inventory each one
/// catches on `a5a61f2`: every row is caught by at least one, and a marker
/// stays only if it is the only one on some row — but for three function
/// words (`che`, `della`, `una`), kept as a net where another marker
/// would do.
const WORDS: &[(&str, u16)] = &[
    // the function words: they cover almost any Italian sentence
    ("non", 49),
    ("il", 47),
    ("di", 35),
    ("la", 35),
    ("da", 29),
    ("un", 13),
    ("una", 7),
    ("del", 8),
    ("dal", 8),
    ("nel", 4),
    ("della", 5),
    ("alla", 5),
    ("che", 6),
    ("senza", 10),
    ("sempre", 4),
    ("perché", 3),
    ("ora", 8),
    // the words the spec names
    ("coda", 13),
    ("nota", 10),
    // the domain
    ("cella", 19),
    ("attesa", 16),
    ("casella", 8),
    ("manca", 8),
    ("stato", 8),
    ("decisioni", 7),
    ("inesistente", 7),
    ("revisione", 7),
    ("riprova", 7),
    ("etichetta", 6),
    ("motivo", 6),
    ("grafo", 5),
    ("ambiti", 4),
    ("ascolto", 4),
    ("modello", 4),
    ("modelli", 1),
    ("politica", 4),
    ("verifica", 2),
    ("verificato", 4),
    ("blocco", 3),
    ("errori", 3),
    ("originale", 3),
    ("portatile", 3),
    ("rango", 3),
    ("revocato", 3),
    ("umano", 3),
    ("agente", 2),
    ("vive", 3),
    ("vivo", 2),
    ("assente", 2),
    ("chiudo", 2),
    ("completato", 2),
    ("coseno", 2),
    ("liste", 2),
    ("procedo", 2),
    ("pulito", 2),
    ("sessione", 2),
    ("stacco", 2),
    ("tutti", 2),
    ("versioni", 2),
    ("al:", 1),
    ("copertura", 1),
    ("creata", 1),
    ("decimale", 1),
    ("diretta", 1),
    ("discordi", 1),
    ("esportate", 1),
    ("fonte", 1),
    ("importate", 1),
    ("incompleto", 1),
    ("indica", 1),
    ("irreversibilmente", 1),
    ("lui", 1),
    ("mai", 1),
    ("punteggio", 1),
    ("rinominati", 1),
    ("saltate", 1),
    ("scritta", 1),
    ("supporto", 1),
];

/// Roots, as substrings — only roots the English language does not
/// contain (`verif` would: verification).
const STEMS: &[(&str, u16)] = &[
    ("annull", 23),
    ("rifiut", 14),
    ("nessun", 22),
    ("richiest", 18),
    ("propost", 16),
    ("conflitt", 13),
    ("accett", 9),
    ("conferm", 6),
    ("dichiarat", 7),
    ("dispositiv", 6),
    ("dimenticat", 5),
    ("aggiunt", 4),
    ("scadut", 4),
    ("aggiornat", 3),
    ("sconosciut", 3),
    ("scaric", 3),
    ("inizializz", 2),
    ("ripristin", 2),
    ("ritentat", 2),
    ("cancellat", 2),
    ("avvis", 6),
    ("mancant", 1),
    ("sostituit", 1),
    ("incorporate:", 5),
    ("<peso>", 2),
];

/// The elided articles (`l'utente`, `dell'…`): a word boundary on the left
/// (`tool's` is not one) and a letter after the apostrophe.
const ARTICLES: &[&str] = &["l'", "dell'", "un'", "sull'", "dall'", "all'", "nell'"];

/// The vowels with a grave accent: English has none (`é` stays out: café).
/// One rule, and every accented word is caught (`è`, `già`, `più`,
/// `validità`).
const ACCENTS: [char; 5] = ['à', 'è', 'ì', 'ò', 'ù'];

/// What belongs to a word: the hyphen too, so that `non-empty`, `diff-del`
/// and `--dry-run` are English whatever their pieces spell.
fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '-'
}

/// Whether `low` (already lowercase) carries the whole word `word`.
fn has_word(low: &str, word: &str) -> bool {
    let mut from = 0;
    while let Some(at) = low[from..].find(word) {
        let start = from + at;
        let end = start + word.len();
        let before = low[..start].chars().next_back();
        let after = low[end..].chars().next();
        if !before.is_some_and(is_word) && !after.is_some_and(is_word) {
            return true;
        }
        from = end.max(start + 1);
    }
    false
}

fn has_article(low: &str, article: &str) -> bool {
    let mut from = 0;
    while let Some(at) = low[from..].find(article) {
        let start = from + at;
        let end = start + article.len();
        let before = low[..start].chars().next_back();
        let letter = low[end..].chars().next().is_some_and(char::is_alphabetic);
        if !before.is_some_and(is_word) && letter {
            return true;
        }
        from = end;
    }
    false
}

/// The Italian markers found in `text`, in the order of the list.
fn markers_in(text: &str) -> Vec<&'static str> {
    let low = text.to_lowercase();
    let mut found = Vec::new();
    for &(word, _) in WORDS {
        if has_word(&low, word) {
            found.push(word);
        }
    }
    for &(stem, _) in STEMS {
        if low.contains(stem) {
            found.push(stem);
        }
    }
    for article in ARTICLES {
        if has_article(&low, article) {
            found.push(article);
        }
    }
    if low.chars().any(|c| ACCENTS.contains(&c)) {
        found.push("à è ì ò ù");
    }
    found
}

// ---------------------------------------------------------------------------
// the ledger
// ---------------------------------------------------------------------------

/// How many surfaces the session read, and the ones still speaking Italian
/// (or breaking what F2 promises beyond the words). Every surface is
/// collected: a red run is the whole list to work from, not the first line.
#[derive(Default)]
struct Gate {
    read: usize,
    red: Vec<String>,
}

impl Gate {
    /// A surface a user or a client reads, as it comes out.
    fn text(&mut self, surface: &str, text: &str) {
        if text.trim().is_empty() {
            return;
        }
        self.read += 1;
        let found = markers_in(text);
        if !found.is_empty() {
            self.red.push(format!(
                "[{surface}] {} — {}",
                found.join(", "),
                quote(text)
            ));
        }
    }

    /// A page. Only what a browser shows is read: the tags, the routes and
    /// the names of the fields are names, and F2.4 decides them. A page
    /// also declares its language.
    fn html(&mut self, surface: &str, body: &str) {
        if body.contains("<html") && (body.contains("lang=\"it\"") || !body.contains("lang=\"en\""))
        {
            self.read += 1;
            let what = if body.contains("lang=\"it\"") {
                "it declares lang=\"it\""
            } else {
                "it declares no language"
            };
            self.red.push(format!(
                "[{surface}] a page must declare lang=\"en\" (F2.4): {what}"
            ));
        }
        self.text(surface, &visible(body));
    }

    /// A promise of F2 that words cannot carry (the routes, the answers).
    fn promise(&mut self, surface: &str, what: &str) {
        self.read += 1;
        self.red.push(format!("[{surface}] {what}"));
    }

    fn finish(self) {
        if !self.red.is_empty() {
            panic!(
                "{} of {} surfaces still speak Italian (or break a promise of F2):\n  {}",
                self.red.len(),
                self.read,
                self.red.join("\n  ")
            );
        }
    }
}

/// The first line that carries a marker, and how many more there are: a
/// red run is a list to work from.
fn quote(text: &str) -> String {
    let mut lines = text.lines().filter(|line| !markers_in(line).is_empty());
    let first: String = lines
        .next()
        .unwrap_or_default()
        .trim()
        .chars()
        .take(120)
        .collect();
    let more = lines.count();
    if more > 0 {
        format!("{first}  (+{more} lines)")
    } else {
        first
    }
}

/// What a browser shows of a page: the tags go, the placeholders stay (a
/// reader sees them), the entities come back.
fn visible(html: &str) -> String {
    let mut out = String::new();
    let mut rest = html;
    while let Some(i) = rest.find('<') {
        out.push_str(&rest[..i]);
        out.push(' '); // a tag ends a word
        let Some(j) = rest[i..].find('>') else {
            break;
        };
        let tag = &rest[i..i + j];
        if let Some(placeholder) = tag
            .split("placeholder=\"")
            .nth(1)
            .and_then(|after| after.split('"').next())
        {
            out.push(' ');
            out.push_str(placeholder);
        }
        rest = &rest[i + j + 1..];
    }
    out.push_str(rest);
    out.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}
// ---------------------------------------------------------------------------
// the CLI: one session through every subcommand
// ---------------------------------------------------------------------------

/// One command of the binary on the session's root. `stdin` is what a
/// command that asks gets to read (nothing: end of file).
fn cli(root: &Path, args: &[&str], stdin: Option<&str>, env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(BIN);
    cmd.arg("--root")
        .arg(root)
        .args(args)
        .envs(env.iter().copied());
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap(/* t */);
    let mut input = child.stdin.take().unwrap(/* t */);
    if let Some(text) = stdin {
        input.write_all(text.as_bytes()).unwrap(/* t */);
    }
    drop(input);
    let out = child.wait_with_output().unwrap(/* t */);
    assert!(
        out.status.code().is_some(),
        "`{args:?}` died on a signal: {out:?}"
    );
    out
}

/// The session's hand on the CLI: it reads both streams of every command.
struct Session<'a> {
    root: &'a Path,
    gate: &'a mut Gate,
}

impl Session<'_> {
    fn read(&mut self, surface: &str, out: &Output, code: i32) {
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(
            out.status.code(),
            Some(code),
            "`{surface}`: wanted exit {code}, got {:?}\nstdout: {stdout}\nstderr: {stderr}",
            out.status.code()
        );
        self.gate.text(&format!("{surface} · stdout"), &stdout);
        self.gate.text(&format!("{surface} · stderr"), &stderr);
    }

    fn run(&mut self, surface: &str, args: &[&str], code: i32) -> Output {
        let out = cli(self.root, args, None, &[]);
        self.read(surface, &out, code);
        out
    }

    /// A command that asks: `answer` is the line the human types.
    fn answer(&mut self, surface: &str, args: &[&str], answer: &str, code: i32) -> Output {
        let out = cli(self.root, args, Some(&format!("{answer}\n")), &[]);
        self.read(surface, &out, code);
        if String::from_utf8_lossy(&out.stdout).contains("[s/N]") {
            self.gate
                .promise(surface, "the prompt says [s/N] (F2.3: [y/N])");
        }
        out
    }

    fn with_env(
        &mut self,
        surface: &str,
        args: &[&str],
        env: &[(&str, &str)],
        code: i32,
    ) -> Output {
        let out = cli(self.root, args, None, env);
        self.read(surface, &out, code);
        out
    }
}

// --- the engine, by hand: the CLI proposes nothing (that is the assistants' job) ---

fn open(root: &Path) -> Cell {
    Cell::open(&root.join("cells/default.sqlite")).unwrap(/* t */)
}

/// A note of the session's English data, as an assistant proposes it.
fn note(body: &str) -> Note {
    let t = At(time::OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap(/* t */));
    Note {
        id: engramma_core::id::new_ulid().unwrap(/* t */),
        note_type: NoteType::Fact,
        title: None,
        body: body.to_string(),
        tags: vec![],
        scope: None,
        lang: Some(Lang::En),
        source: "mcp:claude".into(),
        confidence: Confidence::Stated,
        status: Status::Active,
        valid_from: None,
        valid_to: None,
        supersedes: vec![],
        created_at: t,
        updated_at: t,
        extensions: vec![],
    }
}

fn ulid(text: &str) -> ulid::Ulid {
    ulid::Ulid::from_string(text).unwrap(/* t */)
}

/// Queue a proposal; the id comes back as the CLI prints it.
fn propose(
    root: &Path,
    body: &str,
    mode: ProposalMode,
    client: Option<&str>,
    reason: Option<&str>,
) -> String {
    let cell = open(root);
    cell.propose(
        &ProposalInput {
            note: note(body),
            mode,
            client: client.map(str::to_string),
            reason: reason.map(str::to_string),
        },
        "mcp:claude",
        &ProposeContext::default(),
    )
    .unwrap(/* t */)
    .id
    .expect(/* t */ "a queued proposal has an id")
    .to_string()
}

/// A human note, written straight into the cell.
fn put(root: &Path, body: &str) -> String {
    let cell = open(root);
    let mut n = note(body);
    n.source = "user".into();
    cell.put_note(&n, "cli").unwrap(/* t */);
    n.id.to_string()
}

/// The decision taken on a proposal: its op id.
fn decision_on(root: &Path, proposal: &str) -> String {
    open(root)
        .decisions(&Default::default())
        .unwrap(/* t */)
        .into_iter()
        .find(|d| d.proposal == ulid(proposal))
        .expect(/* t */ "a decision on the proposal")
        .op_id
        .to_string()
}

/// The last word of the first line: the id the CLI prints after its verb,
/// whatever that verb will be.
fn last_word(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .next_back()
        .unwrap_or_default()
        .to_string()
}
/// The CLI: every subcommand with a human output, the `--json` twin where
/// prose travels too, and the errors with their exit codes (2 usage, 3 a
/// missing note, 4 STALE_TARGET). Returns the root the other parts read.
fn cli_session(gate: &mut Gate, dir: &Path) -> PathBuf {
    let root = dir.join("root");
    let mut s = Session { root: &root, gate };
    let file = |name: &str, text: &str| -> String {
        let path = dir.join(name);
        std::fs::write(&path, text).unwrap(/* t */);
        path.to_str().unwrap(/* t */).to_string()
    };

    // --- the root, four notes (one a rumour: born quarantined), reading ---
    s.run("init", &["init"], 0);
    s.run("init --json", &["init", "--json"], 0);
    let ada = last_word(&s.run(
        "add",
        &[
            "add",
            &file(
                "ada.md",
                "Ada Lovelace wrote the first algorithm for the Analytical Engine.",
            ),
            "--source",
            "cli",
            "--confidence",
            "stated",
            "--title",
            "Ada",
        ],
        0,
    ));
    let add = |s: &mut Session, name: &str, body: &str, more: &[&str]| -> String {
        let path = file(name, body);
        let mut args: Vec<&str> = vec!["add", &path, "--source", "cli"];
        args.extend_from_slice(more);
        last_word(&s.run(&format!("add {name}"), &args, 0))
    };
    let grace = add(
        &mut s,
        "grace.md",
        "Grace Hopper coined the word compiler.",
        &["--confidence", "stated"],
    );
    let milan = add(
        &mut s,
        "milan.md",
        "The office is in Milan.",
        &["--confidence", "stated"],
    );
    let genoa = add(
        &mut s,
        "genoa.md",
        "The office is in Genoa.",
        &["--confidence", "stated"],
    );
    let rome = add(
        &mut s,
        "rome.md",
        "The office is in Rome.",
        &["--confidence", "external"],
    );
    s.run("get", &["get", &ada], 0);
    s.run("get (a quarantined note)", &["get", &rome], 0);
    s.run("get --json", &["get", &ada, "--json"], 0);
    s.run(
        "get (a missing note)",
        &["get", "01AAAAAAAAAAAAAAAAAAAAAAAA"],
        3,
    );
    s.run("search", &["search", "compiler"], 0);
    s.run("search --json", &["search", "compiler", "--json"], 0);
    s.run(
        "search (nothing above the threshold)",
        &["search", "xylophone zither"],
        0,
    );

    // --- edit: an editor that changes the text, and one that does not ---
    let editor = file(
        "editor.sh",
        "#!/bin/sh\nsed 's/wrote/wrote down/' \"$1\" > \"$1.new\" && mv \"$1.new\" \"$1\"\n",
    );
    let idle = file("idle.sh", "#!/bin/sh\nexit 0\n");
    {
        use std::os::unix::fs::PermissionsExt;
        for script in [&editor, &idle] {
            std::fs::set_permissions(script, std::fs::Permissions::from_mode(0o755))
                .unwrap(/* t */);
        }
    }
    s.with_env("edit", &["edit", &ada], &[("EDITOR", &editor)], 0);
    s.with_env(
        "edit (nothing changed)",
        &["edit", &ada],
        &[("EDITOR", &idle)],
        0,
    );

    // --- the queue: propose (by hand), list, show, review, accept, reject ---
    s.run("policy set", &["policy", "set", "write=propose"], 0);
    s.run("policy show", &["policy", "show"], 0);
    s.run("proposals (an empty queue)", &["proposals"], 0);
    let p1 = propose(
        &root,
        "Alan Turing cracked the Enigma code.",
        ProposalMode::New,
        Some("claude-desktop 1.2"),
        Some("the user asked to remember it"),
    );
    let _p3 = propose(
        &root,
        "Katherine Johnson calculated the flight paths.",
        ProposalMode::New,
        None,
        None,
    );
    let p2 = propose(
        &root,
        "Ada Lovelace wrote down the notes on the engine.",
        ProposalMode::Supersede(ulid(&ada)),
        None,
        None,
    );
    s.run("proposals", &["proposals"], 0);
    s.run("proposals --json", &["proposals", "--json"], 0);
    s.run("proposals show", &["proposals", "show", &p1], 0);
    s.run("proposals accept", &["proposals", "accept", &p1], 0);
    s.run(
        "proposals reject",
        &["proposals", "reject", &p2, "--reason", "the user denied it"],
        0,
    );
    s.run("decisions", &["decisions"], 0);
    s.run("decisions --json", &["decisions", "--json"], 0);

    // --- revert: the bulk preview, the question, then one decision for real ---
    let d1 = decision_on(&root, &p1);
    s.run(
        "revert in blocco --dry-run",
        &["revert", "--by", "user@cli", "--dry-run"],
        0,
    );
    s.run(
        "revert in blocco --dry-run --json",
        &["revert", "--by", "user@cli", "--dry-run", "--json"],
        0,
    );
    s.answer(
        "revert in blocco (declined)",
        &["revert", "--by", "user@cli"],
        "n",
        0,
    );
    s.run("revert <id> --dry-run", &["revert", &d1, "--dry-run"], 0);
    s.run(
        "revert <id> --dry-run --json",
        &["revert", &d1, "--dry-run", "--json"],
        0,
    );
    s.run("revert <id>", &["revert", &d1], 0);
    s.run("decisions (after the revert)", &["decisions"], 0);
    s.run(
        "revert --by mcp:<actor> (a usage error)",
        &["revert", "--by", "mcp:claude"],
        2,
    );

    // --- STALE_TARGET: a second correction of the same note ---
    let target = put(&root, "The office is in Milan.");
    let first = propose(
        &root,
        "The office is in Rome.",
        ProposalMode::Supersede(ulid(&target)),
        None,
        None,
    );
    let second = propose(
        &root,
        "The office is in Turin.",
        ProposalMode::Supersede(ulid(&target)),
        None,
        None,
    );
    s.run(
        "proposals accept (the first correction)",
        &["proposals", "accept", &first],
        0,
    );
    s.run(
        "proposals accept (STALE_TARGET)",
        &["proposals", "accept", &second],
        4,
    );

    // --- history: the story of a proposal that became a note, and a diff
    // against a later edit (the first correction is active now) ---
    open(&root)
        .patch_note(
            &ulid(&first),
            &NotePatch {
                body: Some("The office moved to Rome.".into()),
                ..Default::default()
            },
            "cli",
        )
        .unwrap(/* t */);
    s.run("history", &["history", &first], 0);
    s.run("history --diff", &["history", &first, "--diff"], 0);

    // --- the automatic review: with `write: auto` and no reviewer configured,
    // every proposal waits for the human, marked; the retry has nothing to do ---
    s.run(
        "policy set (auto)",
        &["policy", "set", "write=auto", "reviewer=bravo"],
        0,
    );
    let _p4 = propose(
        &root,
        "Fred Brooks wrote about the mythical man-month.",
        ProposalMode::New,
        None,
        None,
    );
    s.run("proposals (a failed automatic review)", &["proposals"], 0);
    s.run("proposals review", &["proposals", "review"], 0);
    s.run(
        "policy set (propose again)",
        &["policy", "set", "write=propose"],
        0,
    );

    // --- verify, conflicts ---
    s.run("verify", &["verify"], 0);
    s.run("verify --json", &["verify", "--json"], 0);
    s.run("conflicts", &["conflicts"], 0);
    s.run("conflicts --json", &["conflicts", "--json"], 0);
    s.run(
        "conflicts show (not a copy)",
        &["conflicts", "show", "default", &ada],
        3,
    );

    // --- purge: the warning and the question, then the real ones ---
    s.answer(
        "purge (declined)",
        &["purge", "--reason", "a test", &milan],
        "n",
        0,
    );
    s.run(
        "purge --yes",
        &["purge", &milan, "--yes", "--reason", "a test"],
        0,
    );
    s.answer(
        "purge (answering y)",
        &["purge", "--reason", "a test", &grace],
        "y",
        0,
    );
    if cli(&root, &["get", &grace], None, &[]).status.code() != Some(3) {
        s.gate.promise(
            "purge (answering y)",
            "answering `y` must confirm: the note is still there (F2.3)",
        );
    }
    s.answer(
        "purge (answering s)",
        &["purge", "--reason", "a test", &genoa],
        "s",
        0,
    );
    assert_eq!(
        cli(&root, &["get", &genoa], None, &[]).status.code(),
        Some(3),
        "answering `s` still confirms (F2.3: s stays accepted)"
    );

    // --- sync ---
    s.run("sync status", &["sync", "status"], 0);
    s.run("sync status --json", &["sync", "status", "--json"], 0);
    s.run("sync conflicts", &["sync", "conflicts"], 0);

    // --- the help and the errors of use ---
    s.run("--help", &["--help"], 0);
    s.run("mcp --help", &["mcp", "--help"], 0);
    s.run("an unknown flag (clap)", &["get", "--bogus"], 2);
    s.run("compact (refused)", &["compact"], 2);
    s.run(
        "cell drop (a blank reason)",
        &["cell", "drop", "default", "--reason", " "],
        2,
    );
    s.run(
        "mcp --compat (unknown)",
        &["mcp", "--cells", "default", "--compat", "bogus"],
        2,
    );
    s.run(
        "mcp --compat chatgpt over stdio",
        &["mcp", "--cells", "default", "--compat", "chatgpt"],
        2,
    );
    s.run(
        "mcp --actor without --write",
        &["mcp", "--cells", "default", "--actor", "x"],
        2,
    );
    s.run("oauth deny (no argument)", &["oauth", "deny"], 2);
    s.run("oauth revoke (no argument)", &["oauth", "revoke"], 2);
    s.run("oauth pending (nothing)", &["oauth", "pending"], 0);
    s.run("oauth grants (nothing)", &["oauth", "grants"], 0);
    s.run("oauth log (nothing)", &["oauth", "log"], 0);
    root
}

/// The end of the CLI session: the devices, read after every server is
/// gone (a device key changes how the later writes sign themselves).
fn cli_devices(gate: &mut Gate, root: &Path) {
    let mut s = Session { root, gate };
    s.run("devices init", &["devices", "init"], 0);
    s.run("devices list", &["devices", "list"], 0);
    s.run("devices list --json", &["devices", "list", "--json"], 0);
}
// ---------------------------------------------------------------------------
// a child of the binary that serves
// ---------------------------------------------------------------------------

/// A server of the binary: its stdout lines arrive on a channel (a reader
/// thread, so nobody waits forever on a dead child), its stderr goes to a
/// file the gate reads at the end. Dropping it kills the child: nothing is
/// left running, however the test ends.
struct Server {
    child: Child,
    lines: Receiver<String>,
    stdin: Option<std::process::ChildStdin>,
    err: PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Server {
    /// `talk`: the child reads stdin (the stdio MCP server), or gets none.
    fn spawn(cmd: &mut Command, dir: &Path, name: &str, talk: bool) -> Server {
        let err = dir.join(format!("{name}.stderr.log"));
        let file = std::fs::File::create(&err).unwrap(/* t */);
        let mut child = cmd
            .stdin(if talk {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(file.try_clone().unwrap(/* t */))
            .spawn()
            .unwrap(/* t */);
        drop(file); // the child writes: the gate reopens it to read
        let (tx, rx) = mpsc::channel();
        if let Some(stdout) = child.stdout.take() {
            std::thread::spawn(move || {
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    if tx.send(line).is_err() {
                        break;
                    }
                }
            });
        }
        Server {
            stdin: child.stdin.take(),
            child,
            lines: rx,
            err,
        }
    }

    /// The next line of the server's stdout within the deadline.
    fn line(&self) -> String {
        self.lines
            .recv_timeout(DEADLINE)
            .expect(/* t */ "a line on the server's stdout")
    }

    /// A JSON line from the server (notifications and empty lines skip).
    fn json(&self) -> serde_json::Value {
        loop {
            let line = self.line();
            if line.trim().is_empty() {
                continue;
            }
            return serde_json::from_str(line.trim())
                .unwrap_or_else(|e| panic!("not JSON from the server: {e}: {line}"));
        }
    }

    /// A line to the child's stdin.
    fn say(&mut self, line: &str) {
        let input = self
            .stdin
            .as_mut()
            .expect(/* t */ "this server reads stdin");
        input.write_all(line.as_bytes()).unwrap(/* t */);
        input.write_all(b"\n").unwrap(/* t */);
        input.flush().unwrap(/* t */);
    }

    /// Stop the server — SIGTERM, so a server that drains gets to say so —
    /// then read what it wrote on stderr.
    fn stop(mut self, gate: &mut Gate, surface: &str) {
        let _ = Command::new("sh")
            .arg("-c")
            .arg(format!("kill -TERM {}", self.child.id()))
            .status();
        for _ in 0..100 {
            if self.child.try_wait().unwrap(/* t */).is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let stderr = std::fs::read_to_string(&self.err).unwrap_or_default();
        gate.text(surface, &stderr);
        // dropping `self` kills what SIGTERM left (if anything)
    }
}

// --- loopback HTTP, as the existing tests speak it ---

/// A free loopback port: bound to 0, read, released.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap(/* t */)
        .local_addr()
        .unwrap(/* t */)
        .port()
}

/// The agent that follows no redirect and turns no status into an error:
/// every answer is read as it comes.
fn still() -> ureq::Agent {
    ureq::Agent::config_builder()
        .max_redirects(0)
        .http_status_as_error(false)
        .build()
        .new_agent()
}

/// An answer as the gate reads it: the status, the Location if any, the body.
type Reply = (u16, Option<String>, String);

fn reply(mut r: ureq::http::Response<ureq::Body>) -> Reply {
    let location = r
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let body = r.body_mut().read_to_string().unwrap_or_default();
    (r.status().as_u16(), location, body)
}

fn get(agent: &ureq::Agent, url: &str, headers: &[(&str, &str)]) -> Reply {
    let mut req = agent.get(url);
    for (name, value) in headers {
        req = req.header(*name, *value);
    }
    reply(req.call().unwrap(/* t */))
}

fn post(agent: &ureq::Agent, url: &str, headers: &[(&str, &str)], body: &str) -> Reply {
    let mut req = agent.post(url);
    for (name, value) in headers {
        req = req.header(*name, *value);
    }
    reply(req.send(body.as_bytes()).unwrap(/* t */))
}

fn post_form(
    agent: &ureq::Agent,
    url: &str,
    headers: &[(&str, &str)],
    form: &[(&str, &str)],
) -> Reply {
    let mut req = agent.post(url);
    for (name, value) in headers {
        req = req.header(*name, *value);
    }
    reply(req.send_form(form.to_vec()).unwrap(/* t */))
}

fn post_json(
    agent: &ureq::Agent,
    url: &str,
    headers: &[(&str, &str)],
    json: serde_json::Value,
) -> Reply {
    let mut req = agent.post(url);
    for (name, value) in headers {
        req = req.header(*name, *value);
    }
    reply(req.send_json(json).unwrap(/* t */))
}

/// One raw HTTP exchange over TCP, for a header no honest client sends.
fn raw(port: u16, request: &str) -> (u16, String) {
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap(/* t */);
    stream.write_all(request.as_bytes()).unwrap(/* t */);
    let mut buf = Vec::new();
    let _ = stream.read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf).into_owned();
    let status = text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or_default();
    let body = text
        .split_once("\r\n\r\n")
        .map_or_else(String::new, |(_, body)| body.to_string());
    (status, body)
}

/// Percent-encoding and its reverse, for the URLs of OAuth.
fn urlencode(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                char::from(b).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

fn urldecode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes
            .get(i + 1..i + 3)
            .and_then(|h| std::str::from_utf8(h).ok());
        if bytes[i] == b'%'
            && let Some(value) = hex.and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(value);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---------------------------------------------------------------------------
// the review page
// ---------------------------------------------------------------------------

/// A path with its ids hidden (`/note/01M8…` becomes `/note/<id>`): the
/// red run reads the same from one session to the next. An id is a run of
/// twenty or more uppercase letters and digits (a ULID is twenty-six).
fn shape(path: &str) -> String {
    let mut out = String::new();
    let mut run = String::new();
    for c in path.chars() {
        if c.is_ascii_uppercase() || c.is_ascii_digit() {
            run.push(c);
            continue;
        }
        flush(&mut out, &mut run);
        out.push(c);
    }
    flush(&mut out, &mut run);
    out
}

fn flush(out: &mut String, run: &mut String) {
    if run.len() >= 20 {
        out.push_str("<id>");
    } else {
        out.push_str(run);
    }
    run.clear();
}

/// Every local link a reader can follow (`<a href="/…">`): the stylesheet
/// and the forms are not links.
fn links(html: &str) -> Vec<String> {
    html.split("<a href=\"/")
        .skip(1)
        .map(|after| format!("/{}", after.split('"').next().unwrap_or_default()))
        .collect()
}

/// The GET form of a page (the search): where it goes, and its field's
/// name — whatever the order of the attributes.
fn search_form(html: &str) -> Option<(String, String)> {
    html.split("<form").skip(1).find_map(|after| {
        let (tag, body) = after.split_once('>')?;
        if !tag.contains("method=\"get\"") {
            return None;
        }
        let action = tag
            .split("action=\"")
            .nth(1)?
            .split('"')
            .next()?
            .to_string();
        let field = body.split("name=\"").nth(1)?.split('"').next()?.to_string();
        Some((action, field))
    })
}

/// The local review page: the banner, the entry, every page the entry leads
/// to (the navigation included, and the search), then the pages nobody
/// links to — and the two promises of F2.4 that words cannot carry.
fn review_page(gate: &mut Gate, dir: &Path, root: &Path) {
    let mut cmd = Command::new(BIN);
    cmd.arg("--root").arg(root).args(["review"]);
    let server = Server::spawn(&mut cmd, dir, "review", false);

    // the banner: the address with the token, on the first line
    let banner = server.line();
    gate.text("review · stdout (the banner)", &banner);
    let at = banner
        .find("http://")
        .expect(/* t */ "the address on the banner");
    let address = &banner[at + "http://".len()..];
    let (origin, rest) = address.split_once('/').unwrap_or((address, ""));
    let token = rest.split("token=").nth(1).unwrap_or_default().to_string();
    let port: u16 = origin
        .rsplit(':')
        .next()
        .and_then(|p| p.parse().ok())
        .unwrap_or_default();
    let base = format!("http://{origin}");
    let cookie = format!("engramma_review_{port}={token}");
    let agent = still();

    let entry = format!("/{}", rest.split('?').next().unwrap_or_default());

    // without the token nobody enters
    let (status, _, body) = get(&agent, &format!("{base}{entry}"), &[]);
    assert_eq!(status, 401, "the entry without a token: {body}");
    gate.html("review · the entry without a token", &body);

    // the token opens the door; from there, every page it leads to
    let (status, location, _) = get(&agent, &banner[at..], &[]);
    assert_eq!(status, 303, "the token becomes a cookie");
    let mut queue = vec![location.expect(/* t */ "where the entry goes")];
    let mut seen: Vec<String> = Vec::new();
    let mut pages: Vec<String> = Vec::new();
    while let Some(path) = queue.pop() {
        if seen.contains(&path) {
            continue;
        }
        seen.push(path.clone());
        let (status, _, body) = get(
            &agent,
            &format!("{base}{path}"),
            &[("Cookie", cookie.as_str())],
        );
        assert_eq!(status, 200, "review `{path}` answered {status}: {body}");
        gate.html(&format!("review · {}", shape(&path)), &body);
        pages.push(body.clone());
        let fresh: Vec<String> = links(&body)
            .into_iter()
            .filter(|link| !seen.contains(link) && !queue.contains(link))
            .collect();
        queue.extend(fresh);
        assert!(seen.len() <= 40, "the crawl does not end: {seen:?}");
    }

    // the search: its form says where it goes (and a page without a GET
    // form would leave the search out of the gate, unnoticed)
    let mut searched = false;
    for page in &pages {
        if let Some((action, field)) = search_form(page) {
            searched = true;
            let (status, _, found) = get(
                &agent,
                &format!("{base}{action}?{field}=compiler"),
                &[("Cookie", cookie.as_str())],
            );
            assert_eq!(status, 200, "the search `{action}`: {found}");
            gate.html(&format!("review · {action}?{field}=…"), &found);
        }
    }
    if !searched {
        gate.promise(
            "review · the search",
            "no GET form on any page: the search of the notes is not exercised \
             (the page changed, or the gate's parser did)",
        );
    }

    // the pages nobody links to
    let (status, _, body) = get(
        &agent,
        &format!("{base}/no-such-page"),
        &[("Cookie", cookie.as_str())],
    );
    assert_eq!(status, 404, "a route that does not exist: {body}");
    gate.html("review · a route that does not exist", &body);
    // the page the entry leads to, with a lying Host (raw TCP: no honest
    // client would send it) — the DNS-rebinding fence
    let (status, body) = raw(
        port,
        &format!(
            "GET {page} HTTP/1.1\r\nHost: evil.example:{port}\r\nConnection: close\r\n\r\n",
            page = seen[0]
        ),
    );
    assert_eq!(status, 403, "a lying Host is refused");
    gate.html("review · a lying Host", &body);

    // F2.4: the new routes answer, the old ones redirect (the bookmarks of the beta)
    for (old, new) in [("/coda", "/queue"), ("/note", "/notes")] {
        let (status, _, body) = get(
            &agent,
            &format!("{base}{new}"),
            &[("Cookie", cookie.as_str())],
        );
        if status != 200 {
            gate.promise(
                &format!("review · {new}"),
                &format!("the new route must answer 200: it answers {status}"),
            );
        } else {
            gate.html(&format!("review · {new}"), &body);
        }
        let (status, location, _) = get(
            &agent,
            &format!("{base}{old}"),
            &[("Cookie", cookie.as_str())],
        );
        if status != 308 || !location.as_deref().is_some_and(|to| to.ends_with(new)) {
            gate.promise(
                &format!("review · {old}"),
                &format!(
                    "the old route must answer 308 to {new}: it answers {status} → {}",
                    location.as_deref().unwrap_or("nothing")
                ),
            );
        }
    }
    server.stop(gate, "review · stderr");
}
// ---------------------------------------------------------------------------
// the MCP server over stdio: the list, the confirmation form, the answers
// ---------------------------------------------------------------------------

impl Server {
    fn send(&mut self, value: &serde_json::Value) {
        self.say(&value.to_string());
    }
}

/// The handshake. `forms`: the client renders the confirmation form.
fn mcp_initialize(server: &mut Server, gate: &mut Gate, surface: &str, forms: bool) {
    let capabilities = if forms {
        serde_json::json!({ "elicitation": {} })
    } else {
        serde_json::json!({})
    };
    server.send(&serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18", "capabilities": capabilities,
            "clientInfo": { "name": "english-gate", "version": "0" },
        }
    }));
    let answer = server.json();
    assert_eq!(answer["id"], 1, "the answer to initialize: {answer}");
    gate.text(&format!("{surface} · initialize"), &answer.to_string());
    server.send(&serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
}

/// The tool list: every name and every description, as a model reads them.
fn mcp_tools(server: &mut Server, gate: &mut Gate, surface: &str) {
    server.send(
        &serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}),
    );
    let answer = server.json();
    assert_eq!(answer["id"], 2, "the tool list: {answer}");
    gate.text(&format!("{surface} · tools/list"), &answer.to_string());
}

/// One tool call over stdio. `form` is what the human answers to the
/// confirmation form, when the server asks one.
fn mcp_call(
    server: &mut Server,
    gate: &mut Gate,
    surface: &str,
    id: i64,
    name: &str,
    args: serde_json::Value,
    form: Option<&str>,
) {
    server.send(&serde_json::json!({
        "jsonrpc": "2.0", "id": id, "method": "tools/call",
        "params": { "name": name, "arguments": args.clone() }
    }));
    loop {
        let line = server.json();
        let Some(ask) = line.get("id") else {
            continue; // a notification
        };
        if line.get("method").is_none() {
            assert_eq!(line["id"], id, "{surface}: {line}");
            gate.text(&format!("{surface} · the answer"), &line.to_string());
            return;
        }
        assert_eq!(
            line["method"], "elicitation/create",
            "{surface}: a request the caller did not expect: {line}"
        );
        gate.text(
            &format!("{surface} · the confirmation form"),
            &line["params"].to_string(),
        );
        let action = form.unwrap_or_else(|| panic!("{surface}: the server asked a form: {line}"));
        server.send(&serde_json::json!({
            "jsonrpc": "2.0", "id": ask, "result": { "action": action, "content": {"text": args["text"]} }
        }));
    }
}

fn stdio_server(root: &Path, dir: &Path, name: &str) -> Server {
    let mut cmd = Command::new(BIN);
    cmd.arg("--root").arg(root).args([
        "mcp", "--cells", "default", "--write", "default", "--actor", "claude",
    ]);
    Server::spawn(&mut cmd, dir, name, true)
}

/// The MCP server over stdio, with `write: confirm`: the policy that asks
/// the human through the client's form.
fn mcp_stdio(gate: &mut Gate, dir: &Path, root: &Path) {
    Session {
        root,
        gate: &mut *gate,
    }
    .run(
        "policy set (confirm)",
        &["policy", "set", "write=confirm"],
        0,
    );

    // a client that renders the form: cancelled, declined, accepted
    let surface = "mcp · stdio (with forms)";
    let mut with = stdio_server(root, dir, "mcp-with-forms");
    mcp_initialize(&mut with, gate, surface, true);
    mcp_tools(&mut with, gate, surface);
    mcp_call(
        &mut with,
        gate,
        "mcp · stdio (the form, cancelled)",
        10,
        "memory_propose",
        serde_json::json!({ "text": "Edsger Dijkstra wrote about shortest paths." }),
        Some("cancel"),
    );
    mcp_call(
        &mut with,
        gate,
        "mcp · stdio (the form, declined)",
        11,
        "memory_propose",
        serde_json::json!({
            "text": "Barbara Liskov stated the substitution principle.",
            "reason": "the user said so",
        }),
        Some("decline"),
    );
    mcp_call(
        &mut with,
        gate,
        "mcp · stdio (the form, accepted)",
        12,
        "memory_propose",
        serde_json::json!({ "text": "Ken Thompson wrote the first Go compiler." }),
        Some("accept"),
    );
    mcp_call(
        &mut with,
        gate,
        "mcp · stdio (a missing note)",
        13,
        "memory_get",
        serde_json::json!({ "id": "01AAAAAAAAAAAAAAAAAAAAAAAA" }),
        None,
    );
    with.stop(gate, "mcp · stdio (with forms) · stderr");

    // a client without the form (the ChatGPT shape): the queue, with a
    // pointer to where the human decides
    let surface = "mcp · stdio (without forms)";
    let mut without = stdio_server(root, dir, "mcp-without-forms");
    mcp_initialize(&mut without, gate, surface, false);
    mcp_tools(&mut without, gate, surface);
    mcp_call(
        &mut without,
        gate,
        "mcp · stdio (no form: the queue)",
        20,
        "memory_propose",
        serde_json::json!({ "text": "Margaret Hamilton wrote the flight software." }),
        None,
    );
    mcp_call(
        &mut without,
        gate,
        "mcp · stdio (a search)",
        21,
        "memory_search",
        serde_json::json!({ "query": "algorithm" }),
        None,
    );
    without.stop(gate, "mcp · stdio (without forms) · stderr");
}
// ---------------------------------------------------------------------------
// the MCP server over HTTP: the OAuth pages, the token endpoint, /mcp
// ---------------------------------------------------------------------------

const REDIRECT: &str = "https://client.example.net/callback";
const HOST: &str = "memory.example.net";

/// The approval code out of a waiting page: what `oauth approve` takes.
fn approval_code(html: &str) -> String {
    html.split("engramma oauth approve ")
        .nth(1)
        .map(|rest| {
            rest.chars()
                .take_while(|c| !c.is_whitespace() && *c != '<')
                .collect()
        })
        .unwrap_or_default()
}

/// The authorization code out of the redirect that carries it to the client.
fn code_of(redirect: &str) -> String {
    redirect
        .split("code=")
        .nth(1)
        .and_then(|rest| rest.split('&').next())
        .unwrap_or_default()
        .to_string()
}

/// A PKCE pair: the verifier and its S256 challenge.
fn pkce() -> (String, String) {
    let verifier = "english-gate-verifier-with-enough-characters".to_string();
    let challenge = engramma_mcp::oauth::pkce_s256(&verifier);
    (verifier, challenge)
}

/// The loopback server as a client sees it: every request carries the
/// public Host, exactly as the tunnel in front of it does.
struct Http {
    base: String,
    agent: ureq::Agent,
}

impl Http {
    fn get(&self, path: &str) -> Reply {
        get(
            &self.agent,
            &format!("{}{path}", self.base),
            &[("Host", HOST)],
        )
    }

    fn post(&self, path: &str, headers: &[(&str, &str)], body: &str) -> Reply {
        let mut all: Vec<(&str, &str)> = vec![("Host", HOST)];
        all.extend_from_slice(headers);
        post(&self.agent, &format!("{}{path}", self.base), &all, body)
    }

    fn form(&self, path: &str, form: &[(&str, &str)]) -> Reply {
        post_form(
            &self.agent,
            &format!("{}{path}", self.base),
            &[("Host", HOST)],
            form,
        )
    }

    fn json(&self, path: &str, json: serde_json::Value) -> Reply {
        post_json(
            &self.agent,
            &format!("{}{path}", self.base),
            &[("Host", HOST)],
            json,
        )
    }

    /// A POST to `/mcp`: the answer, and the session the server opened.
    fn mcp(&self, headers: &[(&str, &str)], body: &str) -> (Reply, Option<String>) {
        let mut all: Vec<(&str, &str)> = vec![("Host", HOST)];
        all.extend_from_slice(headers);
        let mut req = self.agent.post(format!("{}/mcp", self.base));
        for (name, value) in all {
            req = req.header(name, value);
        }
        let r = req.send(body.as_bytes()).unwrap(/* t */);
        let session = r
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        (reply(r), session)
    }
}

/// A client registers (RFC 7591, `--allow-dcr`): its id.
fn oauth_register(http: &Http, gate: &mut Gate) -> String {
    let (status, _, body) = http.json(
        "/register",
        serde_json::json!({ "redirect_uris": [REDIRECT], "client_name": "Notebook" }),
    );
    assert_eq!(status, 201, "the registration: {body}");
    gate.text("mcp --http · /register", &body);
    let doc: serde_json::Value = serde_json::from_str(&body).unwrap(/* t */);
    doc["client_id"].as_str().unwrap_or_default().to_string()
}

/// The client asks for a code: the PKCE verifier it will prove, and the
/// waiting page the browser is sent to.
fn oauth_ask(http: &Http, client: &str, state: &str) -> (String, String) {
    let (verifier, challenge) = pkce();
    let query = format!(
        "/authorize?response_type=code&client_id={}&redirect_uri={}&code_challenge={challenge}\
         &code_challenge_method=S256&scope=memory%3Aread%20memory%3Apropose&state={state}",
        urlencode(client),
        urlencode(REDIRECT)
    );
    let (status, location, body) = http.get(&query);
    assert_eq!(status, 303, "to the waiting page: {body}");
    (verifier, location.expect(/* t */ "the waiting page"))
}

/// A client registers, asks for a code, the human approves it from the CLI,
/// the client trades it for tokens and speaks to `/mcp`; then the refusals
/// of the token endpoint. Returns the waiting page, for the page after.
fn http_approval(gate: &mut Gate, root: &Path, http: &Http) -> String {
    // an /authorize without a client: a page, never a redirect
    let (_, _, body) = http.get("/authorize");
    gate.html("mcp --http · /authorize without a client", &body);

    // the registration, the request, the waiting page and its code
    let client = oauth_register(http, gate);
    let (verifier, waiting) = oauth_ask(http, &client, "s1");
    let (status, _, body) = http.get(&waiting);
    assert_eq!(status, 200, "the waiting page: {body}");
    gate.html("mcp --http · the waiting page", &body);
    let code = approval_code(&body);

    // the human decides, from the CLI, on the same root
    {
        let mut s = Session {
            root,
            gate: &mut *gate,
        };
        s.run("oauth pending (a request)", &["oauth", "pending"], 0);
        s.run("oauth pending --json", &["oauth", "pending", "--json"], 0);
        s.run(
            "oauth approve (an unknown code)",
            &["oauth", "approve", "ZZZZ-ZZZZ", "--label", "notebook"],
            3,
        );
        s.run(
            "oauth approve",
            &["oauth", "approve", &code, "--label", "notebook", "--yes"],
            0,
        );
    }

    // back to the client with the code; the client trades it for tokens
    let (status, location, body) = http.get(&waiting);
    assert_eq!(status, 302, "back to the client: {body}");
    let to = location.expect(/* t */ "back to the client");
    assert!(to.starts_with(REDIRECT), "{to}");
    let authorization = code_of(&to);
    let (status, _, body) = http.form(
        "/token",
        &[
            ("grant_type", "authorization_code"),
            ("code", authorization.as_str()),
            ("client_id", client.as_str()),
            ("redirect_uri", REDIRECT),
            ("code_verifier", verifier.as_str()),
        ],
    );
    assert_eq!(status, 200, "the token exchange: {body}");
    gate.text("mcp --http · /token (the tokens)", &body);
    let tokens: serde_json::Value = serde_json::from_str(&body).unwrap(/* t */);
    let bearer = format!(
        "Bearer {}",
        tokens["access_token"].as_str().unwrap_or_default()
    );

    // /mcp: the session opens with initialize, then the tools
    let headers = [
        ("Content-Type", "application/json"),
        ("Accept", "application/json, text/event-stream"),
        ("MCP-Protocol-Version", "2025-06-18"),
        ("Authorization", bearer.as_str()),
    ];
    let initialize = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18", "capabilities": {},
            "clientInfo": { "name": "english-gate", "version": "0" },
        }
    });
    let ((status, _, body), session) = http.mcp(&headers, &initialize.to_string());
    assert_eq!(status, 200, "initialize over HTTP: {body}");
    gate.text("mcp --http · /mcp initialize", &body);
    let mut headers: Vec<(&str, &str)> = headers.to_vec();
    if let Some(id) = session.as_deref() {
        headers.push(("Mcp-Session-Id", id));
    }
    for (surface, call) in [
        (
            "mcp --http · /mcp tools/list",
            serde_json::json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
        ),
        (
            "mcp --http · /mcp tools/call",
            serde_json::json!({
                "jsonrpc": "2.0", "id": 3, "method": "tools/call",
                "params": { "name": "memory_search", "arguments": { "query": "algorithm" } }
            }),
        ),
    ] {
        let (status, _, body) = http.post("/mcp", &headers, &call.to_string());
        assert_eq!(status, 200, "{surface}: {body}");
        gate.text(surface, &body);
    }

    // the token endpoint's own refusals
    let refusals: [(&str, Vec<(&str, &str)>); 3] = [
        ("mcp --http · /token without grant_type", vec![]),
        (
            "mcp --http · /token without a code",
            vec![("grant_type", "authorization_code")],
        ),
        (
            "mcp --http · /token without a refresh token",
            vec![("grant_type", "refresh_token")],
        ),
    ];
    for (surface, form) in refusals {
        let (status, _, body) = http.form("/token", &form);
        assert_eq!(status, 400, "{surface}: {body}");
        gate.text(surface, &body);
    }
    waiting
}

/// A second client is refused by the human (`oauth deny --all`): the page
/// sends the browser back with `access_denied`. Then the registry, the
/// event log and the revocation.
fn http_denial(gate: &mut Gate, root: &Path, http: &Http) {
    let client = oauth_register(http, gate);
    let (_, second) = oauth_ask(http, &client, "s2");
    let (status, _, body) = http.get(&second);
    assert_eq!(status, 200, "the second waiting page: {body}");
    gate.html("mcp --http · the second waiting page", &body);
    let code = approval_code(&body);
    {
        let mut s = Session {
            root,
            gate: &mut *gate,
        };
        s.answer(
            "oauth approve (declined)",
            &["oauth", "approve", &code, "--label", "notebook"],
            "n",
            0,
        );
        s.run(
            "oauth deny (an unknown code)",
            &["oauth", "deny", "ZZZZ-ZZZZ"],
            3,
        );
        s.run("oauth deny --all", &["oauth", "deny", "--all"], 0);
    }
    let (status, location, body) = http.get(&second);
    assert_eq!(status, 302, "back to the client, refused: {body}");
    let to = location.expect(/* t */ "back to the client");
    gate.text("mcp --http · the redirect of a denial", &urldecode(&to));

    {
        let mut s = Session {
            root,
            gate: &mut *gate,
        };
        s.run("oauth grants", &["oauth", "grants"], 0);
        s.run("oauth grants --json", &["oauth", "grants", "--json"], 0);
        s.run("oauth log", &["oauth", "log"], 0);
        s.run("oauth log --json", &["oauth", "log", "--json"], 0);
        s.run("oauth revoke --all", &["oauth", "revoke", "--all"], 0);
        s.run("oauth grants (revoked)", &["oauth", "grants"], 0);
        s.run(
            "oauth revoke (an unknown label)",
            &["oauth", "revoke", "ghost"],
            3,
        );
    }
    let (_, _, body) = http.get("/attesa/0");
    gate.html("mcp --http · an unknown request", &body);
}

/// `engramma mcp --http`: the banner, the OAuth pages, the token endpoint,
/// `/mcp`, and the access log on stderr.
fn mcp_http(gate: &mut Gate, dir: &Path, root: &Path) {
    let addr = format!("127.0.0.1:{}", free_port());
    let mut cmd = Command::new(BIN);
    cmd.arg("--root").arg(root).args([
        "mcp",
        "--cells",
        "default",
        "--write",
        "default",
        "--http",
        addr.as_str(),
        "--public-url",
        "https://memory.example.net",
        "--allow-dcr",
    ]);
    let server = Server::spawn(&mut cmd, dir, "mcp-http", false);
    let banner = server.line();
    gate.text("mcp --http · stdout (the banner)", &banner);
    for _ in 0..200 {
        if std::net::TcpStream::connect(addr.as_str()).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let http = Http {
        base: format!("http://{addr}"),
        agent: still(),
    };
    let waiting = http_approval(gate, root, &http);
    http_denial(gate, root, &http);
    let (_, _, body) = http.get(&waiting);
    gate.html("mcp --http · the waiting page, completed", &body);
    server.stop(gate, "mcp --http · stderr");
}

// ---------------------------------------------------------------------------
// the gate
// ---------------------------------------------------------------------------

#[test]
fn everything_a_user_or_a_client_reads_is_english() {
    let dir = common::test_dir();
    let mut gate = Gate::default();
    let root = cli_session(&mut gate, dir.path());
    mcp_stdio(&mut gate, dir.path(), &root);
    review_page(&mut gate, dir.path(), &root);
    mcp_http(&mut gate, dir.path(), &root);
    cli_devices(&mut gate, &root);
    gate.finish();
}
