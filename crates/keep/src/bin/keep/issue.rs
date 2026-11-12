//! `keep issue` — search, view, file and follow up on keep issues.

use anyhow::Result;

use clap::Subcommand;

use crate::common::TOOL;

/// `keep issue <search|view|create|comment>` — manage keep issues.
/// Positional slots are reserved for the verb + its primary object, so the rest
/// are flags (the CLI convention).
#[derive(clap::Args, Debug)]
pub(crate) struct IssueArgs {
    #[command(subcommand)]
    cmd: IssueCommand,
}

#[derive(Subcommand, Debug)]
enum IssueCommand {
    /// Search keep's issues (`app:keep`); omit the query to list recent.
    Search(IssueSearchArgs),
    /// Print a single issue by number.
    View(IssueViewArgs),
    /// File a structured issue (auto-tagged `app:keep`).
    Create(IssueCreateArgs),
    /// Reopen if needed and add a diagnostics-rich follow-up comment.
    Comment(IssueCommentArgs),
}

/// `keep issue search [query] [--state] [--limit]` flags.
#[derive(clap::Args, Debug)]
struct IssueSearchArgs {
    /// Search text (omit to list recent issues).
    #[arg(num_args = 0..)]
    query: Vec<String>,
    /// Issue state filter.
    #[arg(long, value_parser = ["open", "closed", "all"], default_value = "open")]
    state: String,
    /// Max results.
    #[arg(long, default_value_t = 20)]
    limit: u32,
}

/// `keep issue view <number>` flags.
#[derive(clap::Args, Debug)]
struct IssueViewArgs {
    /// Issue number.
    number: u64,
}

/// `keep issue create [--title <t>] [message...]` flags.
#[derive(clap::Args, Debug)]
struct IssueCreateArgs {
    /// Issue title (default: derived from the message).
    #[arg(long)]
    title: Option<String>,
    /// Print the issue that would be filed (and its URL) without creating it.
    #[arg(long)]
    dry_run: bool,
    /// Free-text description of the problem.
    #[arg(num_args = 0..)]
    message: Vec<String>,
}

/// `keep issue comment <number> [message...]` flags.
#[derive(clap::Args, Debug)]
struct IssueCommentArgs {
    /// Issue number.
    number: u64,
    /// Print the comment/reopen request without changing GitHub state.
    #[arg(long)]
    dry_run: bool,
    /// Free-text follow-up note.
    #[arg(num_args = 0..)]
    message: Vec<String>,
}

/// `keep issue <verb>` — dispatch search/view/create/comment to cli-std.
/// `create` always tags `app:keep`; `search` is filtered to keep's own
/// issues; `comment` reopens before adding the diagnostics-rich note.
pub(crate) async fn run(args: IssueArgs) -> Result<()> {
    match args.cmd {
        IssueCommand::Search(m) => {
            let joined = m.query.join(" ");
            let query = (!joined.trim().is_empty()).then_some(joined);
            cli_std::issue::search(
                &TOOL,
                cli_std::issue::SearchOptions {
                    query,
                    state: m.state,
                    limit: m.limit,
                },
            )
            .await
        }
        IssueCommand::View(m) => cli_std::issue::view(&TOOL, m.number).await,
        IssueCommand::Create(m) => {
            let msg = m.message.join(" ");
            let title = m.title.unwrap_or_else(|| {
                if msg.trim().is_empty() {
                    "keep: issue report".to_string()
                } else {
                    let head: String = msg.lines().next().unwrap_or("").chars().take(72).collect();
                    format!("keep: {head}")
                }
            });
            let message = (!msg.trim().is_empty()).then_some(msg);
            cli_std::issue::create(
                &TOOL,
                cli_std::issue::CreateOptions {
                    title,
                    message,
                    url: None,
                    repo: None,
                    label: vec!["app:keep".to_string()],
                    dry_run: m.dry_run,
                    yes: true,
                },
            )
            .await
        }
        IssueCommand::Comment(m) => {
            let msg = m.message.join(" ");
            cli_std::issue::comment(
                &TOOL,
                cli_std::issue::CommentOptions {
                    number: m.number,
                    message: (!msg.trim().is_empty()).then_some(msg),
                    repo: None,
                    dry_run: m.dry_run,
                    yes: true,
                },
            )
            .await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cli, Command};
    use clap::Parser;

    /// `keep issue search/view/create/comment` parse with their convention flags.
    #[test]
    fn issue_group_parses_search_view_create_comment() {
        let cli = Cli::try_parse_from([
            "keep", "issue", "search", "drain", "--state", "all", "--limit", "5",
        ])
        .expect("issue search should parse");
        match cli.cmd {
            Some(Command::Issue(IssueArgs {
                cmd: IssueCommand::Search(a),
            })) => {
                assert_eq!(a.query, vec!["drain".to_string()]);
                assert_eq!(a.state, "all");
                assert_eq!(a.limit, 5);
            }
            other => panic!("expected issue search, got {other:?}"),
        }

        let cli =
            Cli::try_parse_from(["keep", "issue", "view", "540"]).expect("issue view should parse");
        match cli.cmd {
            Some(Command::Issue(IssueArgs {
                cmd: IssueCommand::View(a),
            })) => assert_eq!(a.number, 540),
            other => panic!("expected issue view, got {other:?}"),
        }

        let cli = Cli::try_parse_from([
            "keep",
            "issue",
            "create",
            "--title",
            "boom",
            "--dry-run",
            "it",
            "broke",
        ])
        .expect("issue create should parse");
        match cli.cmd {
            Some(Command::Issue(IssueArgs {
                cmd: IssueCommand::Create(a),
            })) => {
                assert_eq!(a.title.as_deref(), Some("boom"));
                assert!(a.dry_run);
                assert_eq!(a.message, vec!["it".to_string(), "broke".to_string()]);
            }
            other => panic!("expected issue create, got {other:?}"),
        }

        let cli = Cli::try_parse_from([
            "keep",
            "issue",
            "comment",
            "926",
            "--dry-run",
            "still",
            "failing",
        ])
        .expect("issue comment should parse");
        match cli.cmd {
            Some(Command::Issue(IssueArgs {
                cmd: IssueCommand::Comment(a),
            })) => {
                assert_eq!(a.number, 926);
                assert!(a.dry_run);
                assert_eq!(a.message, vec!["still".to_string(), "failing".to_string()]);
            }
            other => panic!("expected issue comment, got {other:?}"),
        }
    }
}
