//! How a branch is named on screen: short, with the Linear ticket it is named
//! for picked out.

/// First words of branch names that look like `word-123` but are not teams.
const NOT_TEAMS: [&str; 9] = [
    "fix", "feat", "feature", "chore", "bug", "hotfix", "release", "pr", "v",
];

/// Longest branch label, in characters; longer ones lose their end.
const MAX_LABEL: usize = 40;

/// The ticket a branch is named for: `DN-479` from `jakub/dn-479-ontology-m4`.
/// The team key and a number open the last path segment, after any `user/`.
pub fn ticket_id(branch: &str) -> Option<String> {
    let segment = branch.rsplit('/').next()?;
    let (team, rest) = segment.split_once('-')?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let after = rest.get(digits.len()..).unwrap_or("");
    let team_ok = (1..=10).contains(&team.len())
        && team.chars().all(|c| c.is_ascii_alphanumeric())
        && team.starts_with(|c: char| c.is_ascii_alphabetic());
    let ends_ok = after.is_empty() || after.starts_with('-');
    if !team_ok
        || digits.is_empty()
        || digits.len() > 6
        || !ends_ok
        || NOT_TEAMS.contains(&team.to_ascii_lowercase().as_str())
    {
        return None;
    }
    Some(format!("{}-{digits}", team.to_ascii_uppercase()))
}

/// A branch as shown, in the pieces around its ticket so the ticket can link:
/// `feat/` + `ENG-7` + `-thing`.
#[derive(Debug, PartialEq, Eq)]
pub struct Label {
    pub before: String,
    /// Uppercased: `dn-544-x` shows as `DN-544-x`.
    pub ticket: Option<String>,
    pub after: String,
}

/// The branch without the `user/` prefix every branch of `user`'s carries,
/// its end cut when long. The ticket stays whole: it opens the last segment,
/// and the directories before it are cut when they leave it no room.
pub fn label(branch: &str, user: Option<&str>) -> Label {
    let name = user
        .and_then(|u| branch.strip_prefix(u))
        .and_then(|rest| rest.strip_prefix('/'))
        .unwrap_or(branch);
    let Some(ticket) = ticket_id(name) else {
        return Label {
            before: String::new(),
            ticket: None,
            after: truncate(name, MAX_LABEL),
        };
    };
    let (before, segment) = name
        .rsplit_once('/')
        .map_or((String::new(), name), |(dir, segment)| {
            (format!("{dir}/"), segment)
        });
    let after: String = segment.chars().skip(ticket.chars().count()).collect();
    let before = truncate(&before, MAX_LABEL.saturating_sub(ticket.chars().count()));
    let room = MAX_LABEL.saturating_sub(before.chars().count() + ticket.chars().count());
    Label {
        before,
        after: truncate(&after, room),
        ticket: Some(ticket),
    }
}

/// `text` in at most `max` characters, the last of them `…` when cut.
fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let kept: String = text.chars().take(max.saturating_sub(1)).collect();
    format!("{kept}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ticket_is_named_by_the_branchs_last_segment() {
        assert_eq!(
            ticket_id("jakub/dn-479-ontology-m4-base").as_deref(),
            Some("DN-479")
        );
        assert_eq!(ticket_id("DN-12").as_deref(), Some("DN-12"));
        assert_eq!(ticket_id("feature/eng-7-thing").as_deref(), Some("ENG-7"));
    }

    #[test]
    fn branches_without_a_ticket_have_none() {
        for branch in [
            "main",
            "feat/tray-stats-window",
            "fix-123-typo",
            "jakub/dn-notes",
            "dn-12abc",
            "jakub/dn-1234567-x",
            "release/v-1",
            "-5",
        ] {
            assert_eq!(ticket_id(branch), None, "{branch}");
        }
    }

    fn plain(branch: &str, user: Option<&str>) -> String {
        let l = label(branch, user);
        format!("{}{}{}", l.before, l.ticket.unwrap_or_default(), l.after)
    }

    #[test]
    fn a_label_drops_the_users_prefix_and_splits_out_the_ticket() {
        assert_eq!(
            label("marc/dn-544-discovery", Some("marc")),
            Label {
                before: String::new(),
                ticket: Some("DN-544".into()),
                after: "-discovery".into()
            }
        );
        assert_eq!(
            label("feature/eng-7-thing", None),
            Label {
                before: "feature/".into(),
                ticket: Some("ENG-7".into()),
                after: "-thing".into()
            }
        );
        assert_eq!(plain("marc/marketing-site", Some("marc")), "marketing-site");
        assert_eq!(
            plain("jakub/marketing-site", Some("marc")),
            "jakub/marketing-site"
        );
        assert_eq!(
            plain("marcus/x", Some("marc")),
            "marcus/x",
            "only a whole first segment is the user's"
        );
    }

    #[test]
    fn a_long_label_loses_its_end_but_keeps_its_ticket() {
        let long = plain(
            "marc/dn-544-discovery-tables-scans-slack-channels-agent",
            Some("marc"),
        );
        assert_eq!(long, "DN-544-discovery-tables-scans-slack-cha…");
        assert_eq!(long.chars().count(), MAX_LABEL);
        assert_eq!(
            plain("fix-a-very-long-branch-name-for-the-tray-window", None),
            "fix-a-very-long-branch-name-for-the-tra…"
        );
    }

    #[test]
    fn a_long_directory_before_the_ticket_is_cut_too() {
        let long = label(
            "team/area/sub/component/very-long-directory-name/dn-12-x",
            None,
        );
        assert_eq!(
            long,
            Label {
                before: "team/area/sub/component/very-long-…".into(),
                ticket: Some("DN-12".into()),
                after: String::new(),
            }
        );
        let text = format!("{}DN-12", long.before);
        assert_eq!(text.chars().count(), MAX_LABEL);
    }
}
