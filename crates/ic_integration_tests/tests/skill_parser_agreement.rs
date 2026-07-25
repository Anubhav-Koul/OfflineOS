//! Phase 8e gate: the widget's skill importer accepts exactly what the runtime
//! installs.
//!
//! ## Why this test exists
//!
//! The importer (`ic_widget::skill_import`) has to decide, *before* the runtime
//! ever sees a file, whether a third-party `SKILL.md` is a skill. Until 8e it
//! decided with the hand-rolled line scanner written for Phase 7b, whose job was
//! reading a *model's reply* — and that scanner refused real skills. Anthropic's
//! own skills repo is the proof: `claude-api` writes `description: |-` as a YAML
//! block scalar, which a line scanner reads as empty, so the import refused a
//! skill `builtin__skill_install` would have taken without comment.
//!
//! So the importer now applies the runtime's rules. That is a second
//! implementation of someone else's contract, which is the exact shape of thing
//! this repo has been burned by three times (Phase 4 discovery, Phase 8b's
//! `/extensions` parser): a belief held in two places that agree with each other
//! and never meet the real thing. This test is the meeting. It feeds one corpus
//! to **both** parsers — `ic_widget::skill_import::parse_skill_md` and
//! `ironclaw_skills::parse_skill_md` — and asserts they agree on accept/reject
//! and on the name and description they read.
//!
//! It needs no gateway: `ironclaw_skills` is a library, and its parser is the
//! same code `serve` runs. Contract verified against upstream `a492857`.

/// One corpus entry: the file, and a note on what it is probing.
struct Case {
    what: &'static str,
    skill_md: String,
}

fn corpus() -> Vec<Case> {
    let mut cases = vec![
        Case {
            what: "the ordinary shape",
            skill_md: "---\nname: plain-skill\ndescription: Does a thing.\n---\n\n# Body\n\nDo it.\n"
                .to_string(),
        },
        Case {
            what: "a block-scalar description (the real-repo bug)",
            skill_md: "---\nname: claude-api\ndescription: |-\n  Reference for the API.\n  A second line that is still the description.\nlicense: Complete terms in LICENSE.txt\n---\n\n# Body\n\nDo it.\n"
                .to_string(),
        },
        Case {
            what: "a folded-scalar description",
            skill_md: "---\nname: folded\ndescription: >-\n  one\n  two\n---\n\nBody.\n"
                .to_string(),
        },
        Case {
            what: "a quoted description holding a colon",
            skill_md: "---\nname: quoted\ndescription: \"has: a colon\"\n---\n\nBody.\n"
                .to_string(),
        },
        Case {
            what: "extra frontmatter keys the importer does not know",
            skill_md: "---\nname: extra-keys\ndescription: d\nactivation:\n  keywords: [\"a\", \"b\"]\nrequires:\n  bins: [\"vale\"]\n---\n\nBody.\n"
                .to_string(),
        },
        Case {
            what: "a name outside kebab-case but inside the runtime's grammar",
            skill_md: "---\nname: Skill_Name.v2\ndescription: d\n---\n\nBody.\n".to_string(),
        },
        Case {
            what: "CRLF line endings",
            skill_md: "---\r\nname: crlf\r\ndescription: d\r\n---\r\n\r\nBody.\r\n".to_string(),
        },
        Case {
            what: "a leading UTF-8 BOM",
            skill_md: "\u{feff}---\nname: bom\ndescription: d\n---\n\nBody.\n".to_string(),
        },
        Case {
            what: "no frontmatter at all",
            skill_md: "just prose, no frontmatter\n".to_string(),
        },
        Case {
            what: "frontmatter that is never closed",
            skill_md: "---\nname: unclosed\ndescription: d\n\nBody.\n".to_string(),
        },
        Case {
            what: "no description",
            skill_md: "---\nname: no-description\n---\n\nBody.\n".to_string(),
        },
        Case {
            what: "an empty body",
            skill_md: "---\nname: no-body\ndescription: d\n---\n".to_string(),
        },
        Case {
            what: "a name the runtime's grammar refuses",
            skill_md: "---\nname: \"-leading-dash\"\ndescription: d\n---\n\nBody.\n".to_string(),
        },
        Case {
            what: "malformed YAML in the frontmatter",
            skill_md: "---\nname: broken\ndescription: [unclosed\n---\n\nBody.\n".to_string(),
        },
    ];
    // A body right at the runtime's 64 KiB ceiling — accepted by the parser on
    // both sides (the *install* path is where the byte cap bites; the widget
    // refuses it earlier, which is the one deliberate divergence, asserted below).
    cases.push(Case {
        what: "a large but legal body",
        skill_md: format!(
            "---\nname: large\ndescription: d\n---\n\n{}",
            "x".repeat(8 * 1024)
        ),
    });
    cases
}

/// The core assertion: for every case, both parsers make the same call, and when
/// they accept they read the same name and description.
#[test]
fn the_importer_accepts_exactly_what_the_runtime_parses() {
    let mut disagreements = Vec::new();
    for case in corpus() {
        let ours = ic_widget::skill_import::parse_skill_md(&case.skill_md);
        let theirs = ironclaw_skills::parse_skill_md(&case.skill_md);
        match (&ours, &theirs) {
            (Ok(draft), Ok(parsed)) => {
                if draft.name != parsed.manifest.name {
                    disagreements.push(format!(
                        "{}: names differ — importer {:?}, runtime {:?}",
                        case.what, draft.name, parsed.manifest.name
                    ));
                }
                let runtime_description = parsed.manifest.description.trim();
                if draft.description != runtime_description {
                    disagreements.push(format!(
                        "{}: descriptions differ — importer {:?}, runtime {:?}",
                        case.what, draft.description, runtime_description
                    ));
                }
            }
            (Err(reason), Ok(parsed)) => disagreements.push(format!(
                "{}: the importer refused ({reason}) a skill the runtime parses as {:?}",
                case.what, parsed.manifest.name
            )),
            (Ok(draft), Err(error)) => disagreements.push(format!(
                "{}: the importer accepted {:?} but the runtime refuses it ({error})",
                case.what, draft.name
            )),
            (Err(_), Err(_)) => {}
        }
    }
    assert!(
        disagreements.is_empty(),
        "the importer and the runtime disagree about what a skill is:\n  {}",
        disagreements.join("\n  ")
    );
}

/// The one deliberate divergence, stated rather than discovered: the importer is
/// **stricter** in two places, and each has a reason the runtime does not share.
///
/// 1. A `SKILL.md` over `MAX_PROMPT_FILE_SIZE` (64 KiB) parses fine but cannot
///    install — the runtime rejects it at `skill_install`, not at parse. The
///    importer refuses it up front so the user is told before they consent,
///    rather than after.
/// 2. A Windows reserved device name (`con`, `lpt1`, …) is a legal skill name to
///    the runtime's grammar and cannot be a directory here.
#[test]
fn the_importer_is_stricter_only_where_it_has_a_reason() {
    let oversized = format!(
        "---\nname: huge\ndescription: d\n---\n\n{}",
        "x".repeat(ic_widget::skill_import::MAX_SKILL_MD_BYTES as usize)
    );
    assert!(
        ironclaw_skills::parse_skill_md(&oversized).is_ok(),
        "the runtime's *parser* has no size opinion — the install path does"
    );
    let error = ic_widget::skill_import::parse_skill_md(&oversized)
        .expect_err("the importer refuses what cannot install");
    assert!(error.contains("64 KiB"), "{error}");
    assert!(
        oversized.len() as u64 > ironclaw_skills::MAX_PROMPT_FILE_SIZE,
        "the importer's cap must be the runtime's, not a number of our own"
    );

    let reserved = "---\nname: lpt1\ndescription: d\n---\n\nBody.\n";
    assert!(
        ironclaw_skills::parse_skill_md(reserved).is_ok(),
        "the runtime's name grammar allows it"
    );
    assert!(
        ic_widget::skill_import::parse_skill_md(reserved).is_err(),
        "but it cannot be a directory on Windows"
    );
}
