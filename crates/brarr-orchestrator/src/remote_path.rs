//! Translating a download client's paths into brarr's namespace.
//!
//! The client and brarr do not have to see the same filesystem in the
//! same place, and there is no way to make them. This module is the one
//! place that reconciles the two, and it is deliberately pure: every
//! rule below is decidable from two strings.
//!
//! ## Four things this gets right
//!
//! - **A backslash is a legal filename character on Linux.** Replacing
//!   `\` with `/` unconditionally — the usual shortcut — turns the
//!   single directory `AC\DC` into two. Here the separator set is a
//!   function of the detected flavour, and POSIX gets only `/`.
//! - **The longest prefix wins.** Walking the rows and returning the
//!   first match means two overlapping mappings resolve by insertion
//!   order, and the operator has no way to see why.
//! - **A UNC root is a root.** `\\NAS\midia` becomes [`Root::Share`],
//!   and a UNC rule never covers the POSIX path `/NAS/midia/x`.
//! - **`.` and `..` are resolved lexically**, so a `..` never reaches
//!   the join and escapes the local root.
//!
//! Translation **fails open**: a path no rule covers comes back
//! unchanged. The install where brarr and the client share a host and
//! see identical paths needs no mapping at all and has to keep working.
//! Whether the result is usable is decided by the single `metadata`
//! call at the top of [`crate::import`]'s `pick_video`.

use std::path::{Path, PathBuf};

use uuid::Uuid;

use crate::db::path_mappings::PathMapping;

/// Which set of rules a path string was written under.
///
/// Inferred from the string, never from the operating system running
/// brarr — the whole point is that the two can differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathFlavor {
    /// `/data/torrents`. Separator is `/` only.
    Posix,
    /// `C:\Downloads`. Both separators, case-insensitive.
    Windows,
    /// `\\NAS\midia`. Both separators, case-insensitive.
    Unc,
}

impl PathFlavor {
    /// Separators that split a component under this flavour.
    #[must_use]
    fn separators(self) -> &'static [char] {
        match self {
            Self::Posix => &['/'],
            Self::Windows | Self::Unc => &['/', '\\'],
        }
    }

    /// Whether the machine running brarr can open a path written this
    /// way. A Windows path on a Linux host cannot resolve, and knowing
    /// that up front is what turns "does not exist" into a message that
    /// names the real problem.
    #[must_use]
    pub fn is_native(self) -> bool {
        if cfg!(windows) {
            matches!(self, Self::Windows | Self::Unc)
        } else {
            matches!(self, Self::Posix)
        }
    }

    /// Label for the operator-facing message.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Posix => "Unix",
            Self::Windows => "Windows",
            Self::Unc => "rede (UNC)",
        }
    }
}

/// What a path is anchored to. Two paths can only match when their
/// roots match, which is what stops a UNC rule covering a POSIX path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Root<'a> {
    /// Leading `/`.
    Posix,
    /// `C:`.
    Drive(char),
    /// `\\server\share`.
    Share {
        /// Host part.
        server: &'a str,
        /// Share name.
        share: &'a str,
    },
    /// Anchored to nothing — a relative path, or one that walked out of
    /// its own root with `..`. Never equal to anything, including
    /// itself, so it can never match a rule.
    Relative,
}

/// A path string broken into the pieces matching needs.
#[derive(Debug)]
struct Parsed<'a> {
    flavor: PathFlavor,
    root: Root<'a>,
    /// Non-empty components, in order, with `.`/`..` already resolved.
    /// A trailing separator contributes nothing, which is why
    /// `/data/torrents` and `/data/torrents/` are the same prefix.
    segments: Vec<&'a str>,
}

/// `//x` or `\\x` — exactly two identical separators followed by
/// something that is not a separator.
///
/// The forward-slash spelling is treated as UNC on purpose. qBittorrent
/// uses `/` internally and reports an SMB share as `//NAS/share/Rel`,
/// while the operator copies `\\NAS\share` out of Explorer; without
/// this rule the two spellings would never match. On Linux a path
/// starting with exactly two slashes is implementation-defined and
/// essentially does not occur — and if it does, a rule typed the same
/// way parses the same way and still matches. Three or more slashes
/// fall through to POSIX.
fn is_unc_root(raw: &str) -> bool {
    let b = raw.as_bytes();
    b.len() > 2 && matches!(b[0], b'/' | b'\\') && b[0] == b[1] && !matches!(b[2], b'/' | b'\\')
}

/// `C:` / `d:` at the start of the string.
fn has_drive_letter(raw: &str) -> bool {
    let mut chars = raw.chars();
    matches!(
        (chars.next(), chars.next()),
        (Some(c), Some(':')) if c.is_ascii_alphabetic()
    )
}

/// Order matters. UNC first. **POSIX before the backslash test**, or
/// `/data/AC\DC/x` would be classified as Windows, come out rootless,
/// and no rule would cover it — the feature would defeat itself in the
/// exact case it documents getting right. Drive letter next, so that
/// `C:/Downloads` (what one types on a Linux keyboard) is Windows
/// despite having no backslash at all.
fn detect(raw: &str) -> PathFlavor {
    if is_unc_root(raw) {
        return PathFlavor::Unc;
    }
    if raw.starts_with('/') {
        return PathFlavor::Posix;
    }
    if has_drive_letter(raw) || raw.contains('\\') {
        return PathFlavor::Windows;
    }
    PathFlavor::Posix
}

fn split_root(raw: &str, flavor: PathFlavor) -> (Root<'_>, &str) {
    match flavor {
        PathFlavor::Posix => match raw.strip_prefix('/') {
            Some(rest) => (Root::Posix, rest),
            None => (Root::Relative, raw),
        },
        PathFlavor::Windows => {
            // A drive letter only anchors when a separator follows it.
            // `D:\foo` is rooted at D:; `D:foo` is relative to whatever
            // directory that drive is currently sitting in, which names
            // no fixed place and so cannot be rewritten.
            let anchored =
                has_drive_letter(raw) && matches!(raw.as_bytes().get(2), Some(b'/' | b'\\') | None);
            if anchored {
                // The first two characters are ASCII by construction, so
                // index 2 is a char boundary.
                let letter = raw.chars().next().unwrap_or('?');
                (Root::Drive(letter), &raw[2..])
            } else {
                // `\Downloads` is relative to the current drive: it
                // names no volume, so there is nothing to hang the
                // rewrite on.
                (Root::Relative, raw)
            }
        }
        PathFlavor::Unc => {
            let body = raw.trim_start_matches(['\\', '/']);
            let mut parts = body.splitn(3, ['\\', '/']);
            match (parts.next(), parts.next()) {
                (Some(server), Some(share)) if !server.is_empty() && !share.is_empty() => {
                    (Root::Share { server, share }, parts.next().unwrap_or(""))
                }
                // `\\NAS` alone names a host, not a share.
                _ => (Root::Relative, raw),
            }
        }
    }
}

fn parse(raw: &str) -> Parsed<'_> {
    let trimmed = raw.trim();
    let flavor = detect(trimmed);
    let (mut root, rest) = split_root(trimmed, flavor);
    let mut segments: Vec<&str> = Vec::new();
    for fragment in rest.split(flavor.separators()) {
        match fragment {
            "" | "." => {}
            ".." => {
                // Walking out of the root makes the path unanchored, and
                // an unanchored path never matches — rather than
                // becoming dangerous.
                if segments.pop().is_none() {
                    root = Root::Relative;
                }
            }
            other => segments.push(other),
        }
    }
    Parsed {
        flavor,
        root,
        segments,
    }
}

/// Root equality. Case folding follows the namespace: drive letters and
/// SMB server/share names are insensitive, a POSIX root has no name to
/// fold. [`Root::Relative`] is never equal to anything, not even
/// itself.
fn root_eq(a: Root<'_>, b: Root<'_>) -> bool {
    match (a, b) {
        (Root::Posix, Root::Posix) => true,
        (Root::Drive(x), Root::Drive(y)) => x.eq_ignore_ascii_case(&y),
        (
            Root::Share {
                server: s1,
                share: h1,
            },
            Root::Share {
                server: s2,
                share: h2,
            },
        ) => s1.eq_ignore_ascii_case(s2) && h1.eq_ignore_ascii_case(h2),
        _ => false,
    }
}

/// Component equality.
///
/// POSIX is case-sensitive and **has to be**: `/data/AC` and `/data/ac`
/// are two different directories on ext4, and folding would rewrite to
/// the wrong one. Windows and SMB are insensitive. ASCII only: full
/// Unicode folding is locale-dependent, and this is a path, not a
/// search box.
fn segment_eq(a: &str, b: &str, flavor: PathFlavor) -> bool {
    match flavor {
        PathFlavor::Posix => a == b,
        PathFlavor::Windows | PathFlavor::Unc => a.eq_ignore_ascii_case(b),
    }
}

/// How many components of `path` the `prefix` consumes, or `None` when
/// it does not cover `path`.
///
/// The boundary is structural, not textual: components are compared one
/// by one, so `/data/down` cannot cover `/data/downloads/x`
/// (`"down" != "downloads"`). A textual prefix would get exactly that
/// pair wrong.
///
/// [`root_eq`] implies flavour equality by construction of the parse
/// ([`Root::Posix`] only comes from POSIX, [`Root::Drive`] only from
/// Windows, [`Root::Share`] only from UNC), so the fold can safely use
/// the prefix's flavour.
fn covers(prefix: &Parsed<'_>, path: &Parsed<'_>) -> Option<usize> {
    if !root_eq(prefix.root, path.root) {
        return None;
    }
    if prefix.segments.len() > path.segments.len() {
        return None;
    }
    for (want, got) in prefix.segments.iter().zip(path.segments.iter()) {
        if !segment_eq(want, got, prefix.flavor) {
            return None;
        }
    }
    Some(prefix.segments.len())
}

/// The rule that fired, for the log and for the operator's message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedRule {
    /// Id of the row that matched.
    pub id: Uuid,
    /// Its remote side.
    pub remote_prefix: String,
    /// Its local side.
    pub local_prefix: PathBuf,
}

/// The result of translating a client-reported path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Translation {
    /// The path to hand to the filesystem.
    pub local: PathBuf,
    /// Which mapping produced it. `None` means the path came through
    /// untouched — either because no mapping is configured, or because
    /// none covers this prefix.
    pub applied: Option<AppliedRule>,
    /// Namespace the client's string was written in.
    pub flavor: PathFlavor,
    /// `false` for a relative path. A relative path is never rewritten
    /// **and must never be handed to `std::fs`**: it would resolve
    /// against the process working directory, which in Docker is `/`.
    pub rooted: bool,
}

impl Translation {
    /// `true` when nothing rewrote the path *and* the namespace it is
    /// written in is not one this machine can open — a Windows path on
    /// a Linux brarr. It is certain to fail; knowing that beforehand is
    /// what turns "does not exist" into a message naming the real
    /// problem.
    ///
    /// Always `false` once a mapping applied: a mapping's local side is
    /// validated against this machine at registration.
    #[must_use]
    pub fn is_foreign(&self) -> bool {
        self.applied.is_none() && !self.flavor.is_native()
    }
}

/// Append an already-split component.
///
/// This is where the translation between operating systems happens, and
/// it is why the tail is kept as loose components rather than as a
/// path: the tail carries no separator, so gluing it onto the local
/// prefix adopts *this* machine's separator automatically.
///
/// `PathBuf::push` does **not** work here: a component that looks
/// absolute (`D:` is a legal directory name on Linux) would replace the
/// whole path instead of extending it. Concatenating the `OsString`
/// cannot do that.
fn append_component(base: PathBuf, segment: &str) -> PathBuf {
    let mut raw = base.into_os_string();
    let needs_separator = !raw.is_empty() && !raw.to_string_lossy().ends_with(['/', '\\']);
    if needs_separator {
        raw.push(std::path::MAIN_SEPARATOR_STR);
    }
    raw.push(segment);
    PathBuf::from(raw)
}

/// Rewrite a client-reported path into brarr's namespace.
///
/// **Fails open**: a path no mapping covers comes back unchanged. That
/// is not laziness — the layout where brarr and the client share a host
/// and see identical paths needs no mapping at all and has to keep
/// working; and a wrong mapping has to heal the queue the moment it is
/// corrected, which cannot happen if the grab was already failed. The
/// caller is told what happened ([`Translation::applied`],
/// [`Translation::is_foreign`], [`Translation::rooted`]) so it can
/// describe whatever failure follows.
///
/// **No disk probe here.** Once a rule matches, it stands: nothing that
/// happens to exist on brarr's filesystem can contradict what the
/// operator declared. (This operator's container *has* a `/data` — it
/// is the sqlite volume. A fallback to the raw path would reopen the
/// hole this closes.)
#[must_use]
pub fn translate(mappings: &[PathMapping], reported: &str) -> Translation {
    let path = parse(reported);
    let rooted = !matches!(path.root, Root::Relative);
    let mut best: Option<(usize, &PathMapping)> = None;

    for candidate in mappings {
        let prefix = parse(&candidate.remote_prefix);
        let Some(depth) = covers(&prefix, &path) else {
            continue;
        };
        let wins = match best {
            None => true,
            // Longest wins. The tie-break is the prefix string itself
            // rather than database order, so two rows differing only in
            // case resolve the same way on every run.
            Some((current_depth, current)) => {
                depth > current_depth
                    || (depth == current_depth && candidate.remote_prefix < current.remote_prefix)
            }
        };
        if wins {
            best = Some((depth, candidate));
        }
    }

    match best {
        Some((depth, row)) => {
            let mut local = row.local_prefix.clone();
            for segment in path.segments.iter().skip(depth) {
                local = append_component(local, segment);
            }
            Translation {
                local,
                applied: Some(AppliedRule {
                    id: row.id,
                    remote_prefix: row.remote_prefix.clone(),
                    local_prefix: row.local_prefix.clone(),
                }),
                flavor: path.flavor,
                rooted,
            }
        }
        // Nothing matched, so the path passes through — but *normalised*,
        // not raw. Fail-open still hands this string to `std::fs`, and
        // the raw form can carry `..` that the parse already resolved
        // away for matching purposes. Handing back the unresolved
        // spelling would mean the traversal brarr refused to follow when
        // choosing a rule gets followed anyway when opening the file.
        //
        // An unanchored path keeps its original spelling: it is refused
        // upstream and never opened, and showing the operator exactly
        // what the client said is worth more than tidying it.
        None => Translation {
            local: if rooted {
                rebuild(&path)
            } else {
                PathBuf::from(reported.trim())
            },
            applied: None,
            flavor: path.flavor,
            rooted,
        },
    }
}

/// Reassemble a parsed path in its own flavour's spelling, with `.` and
/// `..` already gone. Only ever called for an anchored path.
fn rebuild(parsed: &Parsed<'_>) -> PathBuf {
    let (root, separator) = match parsed.root {
        Root::Posix => ("/".to_owned(), "/"),
        Root::Drive(letter) => (format!("{letter}:\\"), "\\"),
        Root::Share { server, share } => (format!(r"\\{server}\{share}\"), "\\"),
        // Unreachable by the caller's guard; returning the root alone is
        // still the safest thing to hand back.
        Root::Relative => (String::new(), "/"),
    };
    PathBuf::from(root + &parsed.segments.join(separator))
}

/// Rewrite an operator-typed prefix into the canonical form the table
/// stores and the matching compares.
///
/// Trims, collapses repeated separators, resolves `.`/`..`, drops the
/// trailing separator, and writes separators in its own flavour's
/// style. It does **not** fold case and does **not** change flavour:
/// `C:/Downloads` becomes `C:\Downloads`, never `/c/downloads`.
///
/// `None` for a prefix that cannot serve as one:
///
/// - relative — it would never match anything, so storing it stores a
///   mapping that silently does nothing;
/// - the bare POSIX root (`/`) — it would cover every path the client
///   could report and turn one mapping into a global rewrite.
///
/// A bare `C:\` or a bare `\\NAS\midia` are **not** refused: they are
/// anchored by their own root, and "the client's C: is my /mnt/c" is a
/// real and sane mapping.
#[must_use]
pub fn canonical_prefix(raw: &str) -> Option<String> {
    let parsed = parse(raw);
    if matches!(parsed.root, Root::Relative)
        || (matches!(parsed.root, Root::Posix) && parsed.segments.is_empty())
    {
        return None;
    }
    // `root` already ends with its separator for the two flavours whose
    // root is written that way (`/`, `C:\`); a share root is not, so it
    // needs one inserted before the first segment. Getting this wrong
    // leaves a trailing separator on a bare root, and a stored prefix
    // that does not round-trip through the matching.
    let (mut out, separator, separate) = match parsed.root {
        Root::Posix => ("/".to_owned(), "/", false),
        Root::Drive(letter) => (format!("{}:\\", letter.to_ascii_uppercase()), "\\", false),
        Root::Share { server, share } => (format!(r"\\{server}\{share}"), "\\", true),
        Root::Relative => return None,
    };
    if !parsed.segments.is_empty() {
        if separate {
            out.push('\\');
        }
        out.push_str(&parsed.segments.join(separator));
    }
    Some(out)
}

/// How many components a prefix pins — the number "longest wins"
/// compares. `0` for a prefix anchoring only a root. Exposed so the
/// admin table can list mappings in the order they actually resolve.
#[must_use]
pub fn specificity(prefix: &str) -> usize {
    parse(prefix).segments.len()
}

/// The flavour of a string, for the operator's message.
#[must_use]
pub fn flavor_of(raw: &str) -> PathFlavor {
    detect(raw.trim())
}

/// Whether a path can be handed to `std::fs` at all — anchored, and in
/// a namespace this machine understands.
#[must_use]
pub fn is_usable(translation: &Translation) -> bool {
    translation.rooted && !translation.is_foreign()
}

/// Convenience for callers that only want the path.
#[must_use]
pub fn local_path(mappings: &[PathMapping], reported: &str) -> PathBuf {
    translate(mappings, reported).local
}

/// Whether `path` sits under `root`, used by the admin screen to warn
/// about a local side that no root folder can reach.
#[must_use]
pub fn is_under(path: &Path, root: &Path) -> bool {
    path.starts_with(root)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests assert on happy paths"
)]
mod tests {
    use super::*;
    use time::OffsetDateTime;

    /// Build a mapping without a database. `local` is kept as typed, so
    /// the expectations below can be written with `join` and stay
    /// correct on both separators.
    fn rule(remote: &str, local: &str) -> PathMapping {
        PathMapping {
            id: Uuid::nil(),
            client_id: Uuid::nil(),
            remote_prefix: remote.to_owned(),
            local_prefix: PathBuf::from(local),
            created_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    fn matched(mappings: &[PathMapping], reported: &str) -> bool {
        translate(mappings, reported).applied.is_some()
    }

    // ---------------------------------------------------------------
    // The incident this whole module exists for.
    // ---------------------------------------------------------------

    #[test]
    fn the_production_incident_translates() {
        let rules = vec![rule("/data", "/midias")];
        let out = translate(
            &rules,
            "/data/torrents/Tomb Raider King S01E01/Tomb Raider King S01E01.mkv",
        );
        assert_eq!(
            out.local,
            PathBuf::from("/midias")
                .join("torrents")
                .join("Tomb Raider King S01E01")
                .join("Tomb Raider King S01E01.mkv")
        );
        assert!(out.applied.is_some());
    }

    // ---------------------------------------------------------------
    // Component boundary — the case a textual prefix gets wrong.
    // ---------------------------------------------------------------

    #[test]
    fn a_prefix_must_end_on_a_component_boundary() {
        let rules = vec![rule("/data/down", "/midias")];
        assert!(
            !matched(&rules, "/data/downloads/x.mkv"),
            "\"down\" must not cover \"downloads\""
        );
        assert!(matched(&rules, "/data/down/x.mkv"));
    }

    // ---------------------------------------------------------------
    // Longest wins, in both insertion orders.
    // ---------------------------------------------------------------

    #[test]
    fn the_longest_prefix_wins_regardless_of_row_order() {
        let broad = rule("/data", "/wrong");
        let narrow = rule("/data/torrents", "/right");

        for rules in [
            vec![broad.clone(), narrow.clone()],
            vec![narrow.clone(), broad.clone()],
        ] {
            let out = translate(&rules, "/data/torrents/X/f.mkv");
            assert_eq!(
                out.local,
                PathBuf::from("/right").join("X").join("f.mkv"),
                "the more specific rule has to win in either order"
            );
        }
    }

    // ---------------------------------------------------------------
    // Mixed separators — a Windows client reporting to a Linux brarr.
    // ---------------------------------------------------------------

    #[test]
    fn a_windows_path_splits_on_both_separators() {
        let rules = vec![rule(r"C:\Downloads", "/midias/win")];
        let out = translate(&rules, r"C:/Downloads\Filme/f.mkv");
        assert_eq!(
            out.local,
            PathBuf::from("/midias/win").join("Filme").join("f.mkv"),
            "a Windows path mixes separators freely and both must split"
        );
    }

    #[test]
    fn a_windows_rule_may_be_typed_with_forward_slashes() {
        let rules = vec![rule("D:/downloads", "/midias/d")];
        assert!(
            matched(&rules, r"D:\downloads\Rel"),
            "the operator types the rule on a Linux keyboard"
        );
    }

    #[test]
    fn a_drive_letter_folds_case_on_both_sides() {
        let rules = vec![rule(r"c:\downloads", "/midias/d")];
        assert!(matched(&rules, r"C:\DOWNLOADS\x"));
    }

    // ---------------------------------------------------------------
    // A backslash is a legal filename character on Linux. This is the
    // case the usual `replace('\\', '/')` shortcut corrupts.
    // ---------------------------------------------------------------

    #[test]
    fn a_posix_path_splits_only_on_slash() {
        let rules = vec![rule("/data/we", "/midias")];
        assert!(
            !matched(&rules, r"/data/we\ird/f.mkv"),
            r"`we\ird` is one component on Linux, so `/data/we` must not cover it"
        );
    }

    #[test]
    fn a_posix_directory_containing_a_backslash_survives_translation() {
        let rules = vec![rule("/data", "/midias")];
        let out = translate(&rules, r"/data/AC\DC/live.mkv");
        // The tail must still be two components, not three: the rule
        // consumed `data`, leaving `AC\DC` and `live.mkv`.
        assert_eq!(
            out.local,
            PathBuf::from("/midias").join(r"AC\DC").join("live.mkv")
        );
    }

    // ---------------------------------------------------------------
    // UNC roots.
    // ---------------------------------------------------------------

    #[test]
    fn a_unc_share_matches_in_either_spelling() {
        let rules = vec![rule(r"\\NAS\midia", "/mnt/nas")];
        let expected = PathBuf::from("/mnt/nas").join("tv").join("x.mkv");

        assert_eq!(translate(&rules, r"\\NAS\midia\tv\x.mkv").local, expected);
        assert_eq!(
            translate(&rules, "//NAS/midia/tv/x.mkv").local,
            expected,
            "qBittorrent reports a share with forward slashes"
        );
    }

    #[test]
    fn a_unc_rule_never_covers_a_posix_path() {
        let rules = vec![rule(r"\\NAS\midia", "/mnt/nas")];
        assert!(
            !matched(&rules, "/NAS/midia/x.mkv"),
            "a share root and a POSIX root are different anchors"
        );
    }

    #[test]
    fn the_server_name_is_part_of_the_share_root() {
        let rules = vec![rule(r"\\OTHER\midia", "/mnt/other")];
        assert!(!matched(&rules, r"\\NAS\midia\x"));
    }

    // ---------------------------------------------------------------
    // POSIX case sensitivity — deliberately not folded.
    // ---------------------------------------------------------------

    #[test]
    fn posix_components_are_case_sensitive() {
        let rules = vec![rule("/data", "/midias")];
        assert!(
            !matched(&rules, "/Data/x"),
            "/data and /Data are two directories on ext4"
        );
    }

    // ---------------------------------------------------------------
    // Separator noise.
    // ---------------------------------------------------------------

    #[test]
    fn trailing_and_doubled_separators_do_not_change_a_prefix() {
        let rules = vec![rule("/data/torrents/", "/midias/t")];
        assert_eq!(
            translate(&rules, "/data/torrents//X/f.mkv").local,
            PathBuf::from("/midias/t").join("X").join("f.mkv")
        );
    }

    #[test]
    fn an_exact_match_yields_the_local_prefix_untouched() {
        let rules = vec![rule("/data/torrents", "/midias/t")];
        // A single-file torrent: content_path is the file itself, and
        // the whole path is consumed by the rule.
        assert_eq!(
            translate(&rules, "/data/torrents").local,
            PathBuf::from("/midias/t")
        );
    }

    // ---------------------------------------------------------------
    // Anchoring: a relative path is never rewritten and never usable.
    // ---------------------------------------------------------------

    #[test]
    fn a_relative_path_never_matches_and_is_never_usable() {
        let rules = vec![rule("/data", "/midias")];
        let out = translate(&rules, "torrents/X");
        assert!(out.applied.is_none());
        assert!(!out.rooted);
        assert!(
            !is_usable(&out),
            "handing this to std::fs would resolve against the process cwd, which is / in Docker"
        );
    }

    #[test]
    fn a_drive_relative_path_is_not_anchored() {
        let rules = vec![rule(r"C:\Downloads", "/midias")];
        let out = translate(&rules, r"\Downloads\x");
        assert!(out.applied.is_none());
        assert!(!out.rooted, r"`\Downloads` names no volume");
    }

    #[test]
    fn a_drive_qualified_relative_path_is_not_anchored() {
        let rules = vec![rule(r"D:\foo", "/midias")];
        let out = translate(&rules, "D:foo");
        assert!(!out.rooted, "`D:foo` is relative to the drive's cwd");
    }

    // ---------------------------------------------------------------
    // `..` is resolved lexically, before anything reaches the join.
    // ---------------------------------------------------------------

    #[test]
    fn dot_dot_is_resolved_before_matching_and_cannot_escape() {
        let rules = vec![rule("/data", "/midias")];
        let out = translate(&rules, "/data/t/../../etc/passwd");
        assert!(
            out.applied.is_none(),
            "after resolution the path is /etc/passwd, which /data does not cover"
        );
        assert!(
            !out.local.to_string_lossy().contains(".."),
            "no `..` may reach the filesystem"
        );
    }

    #[test]
    fn dot_dot_past_the_root_unanchors_the_path() {
        let rules = vec![rule("/data", "/midias")];
        let out = translate(&rules, "/../../data/x");
        assert!(!out.rooted);
        assert!(out.applied.is_none());
    }

    // ---------------------------------------------------------------
    // Fail open — the layout that needs no mapping at all.
    // ---------------------------------------------------------------

    #[test]
    fn with_no_mappings_a_path_comes_through_untouched() {
        let out = translate(&[], "/downloads/complete/X/f.mkv");
        assert_eq!(out.local, PathBuf::from("/downloads/complete/X/f.mkv"));
        assert!(out.applied.is_none());
        assert!(out.rooted);
    }

    #[test]
    fn an_uncovered_path_comes_through_untouched() {
        let rules = vec![rule("/data", "/midias")];
        let out = translate(&rules, "/other/place/f.mkv");
        assert_eq!(out.local, PathBuf::from("/other/place/f.mkv"));
        assert!(out.applied.is_none());
    }

    // ---------------------------------------------------------------
    // A local prefix is never re-serialised, only appended to.
    // ---------------------------------------------------------------

    #[test]
    fn a_unc_local_prefix_is_preserved_literally() {
        let rules = vec![rule("/data", r"\\NAS\media")];
        let out = translate(&rules, "/data/Rel/f.mkv");
        assert!(
            out.local.to_string_lossy().starts_with(r"\\NAS\media"),
            "the local side is written by the operator and must survive verbatim, got {}",
            out.local.display()
        );
    }

    #[test]
    fn a_tail_component_that_looks_absolute_extends_rather_than_replaces() {
        let rules = vec![rule("/data", "/midias")];
        // `D:` is a legal directory name on Linux. `PathBuf::push` would
        // throw away `/midias` here.
        let out = translate(&rules, "/data/D:/f.mkv");
        assert!(
            out.local.to_string_lossy().starts_with("/midias"),
            "got {}",
            out.local.display()
        );
    }

    // ---------------------------------------------------------------
    // canonical_prefix
    // ---------------------------------------------------------------

    #[test]
    fn canonical_prefix_refuses_what_cannot_be_a_prefix() {
        assert_eq!(canonical_prefix("/"), None, "would match everything");
        assert_eq!(canonical_prefix("Downloads"), None, "relative");
        assert_eq!(canonical_prefix("   "), None);
    }

    #[test]
    fn canonical_prefix_normalises_without_changing_flavour() {
        assert_eq!(
            canonical_prefix("/data//torrents/./"),
            Some("/data/torrents".to_owned())
        );
        assert_eq!(
            canonical_prefix("C:/Downloads/"),
            Some(r"C:\Downloads".to_owned()),
            "canonicalised to Windows spelling, not translated to POSIX"
        );
        assert_eq!(
            canonical_prefix(r"\\nas\Midia\tv\"),
            Some(r"\\nas\Midia\tv".to_owned())
        );
    }

    #[test]
    fn canonical_prefix_allows_a_bare_anchored_root() {
        assert_eq!(canonical_prefix(r"C:\"), Some(r"C:\".to_owned()));
        assert_eq!(
            canonical_prefix(r"\\NAS\midia"),
            Some(r"\\NAS\midia".to_owned())
        );
    }

    #[test]
    fn a_canonical_prefix_round_trips_through_matching() {
        // Whatever canonical_prefix stores must still match the path it
        // was derived from — otherwise the table stores rules that
        // silently never fire.
        for raw in ["/data/torrents/", "C:/Downloads", r"\\NAS\midia\tv"] {
            let canonical = canonical_prefix(raw).unwrap();
            let rules = vec![rule(&canonical, "/local")];
            assert!(
                matched(&rules, raw),
                "canonical form {canonical:?} must still cover {raw:?}"
            );
        }
    }

    #[test]
    fn specificity_counts_pinned_components() {
        assert_eq!(specificity("/"), 0);
        assert_eq!(specificity("/data"), 1);
        assert_eq!(specificity("/data/torrents"), 2);
        assert_eq!(specificity(r"\\NAS\midia"), 0, "the share itself is root");
    }

    // ---------------------------------------------------------------
    // Flavour detection — the ordering trap.
    // ---------------------------------------------------------------

    #[test]
    fn a_posix_path_containing_a_backslash_is_still_posix() {
        assert_eq!(
            flavor_of(r"/data/AC\DC/x"),
            PathFlavor::Posix,
            "testing for a backslash before testing for a leading slash \
             would classify this as Windows and defeat the feature"
        );
    }

    #[test]
    fn flavours_are_detected_from_the_string_not_the_host() {
        assert_eq!(flavor_of("/data/torrents"), PathFlavor::Posix);
        assert_eq!(flavor_of(r"C:\Downloads"), PathFlavor::Windows);
        assert_eq!(flavor_of("C:/Downloads"), PathFlavor::Windows);
        assert_eq!(flavor_of(r"\\NAS\midia"), PathFlavor::Unc);
        assert_eq!(flavor_of("//NAS/midia"), PathFlavor::Unc);
        assert_eq!(flavor_of("///data"), PathFlavor::Posix, "three is not UNC");
    }

    #[test]
    fn a_foreign_path_is_flagged_only_when_nothing_rewrote_it() {
        let foreign = if cfg!(windows) {
            "/data/torrents/x"
        } else {
            r"C:\Downloads\x"
        };
        assert!(
            translate(&[], foreign).is_foreign(),
            "an untranslated path from another namespace is certain to fail"
        );

        let rules = vec![rule(
            if cfg!(windows) {
                "/data"
            } else {
                r"C:\Downloads"
            },
            "/midias",
        )];
        assert!(
            !translate(&rules, foreign).is_foreign(),
            "once a rule applies, the local side is this machine's"
        );
    }
}
