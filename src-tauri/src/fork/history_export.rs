//! Writing the transcription history out as a file the user keeps.
//!
//! The whole export is built in Rust rather than in the page. The page loads
//! history a screen at a time and caps a query at 100 rows, so an export driven
//! from there would either be a pagination loop or, worse, quietly contain only
//! what had been scrolled past. Formatting here also keeps a document of some
//! hundreds of entries off the IPC boundary and makes both formatters testable
//! without a window.

use crate::managers::history::{HistoryEntry, HistoryManager};
use chrono::{DateTime, Local, SecondsFormat, Utc};
use serde::Serialize;
use specta::Type;
use std::sync::Arc;
use tauri::{AppHandle, State};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum HistoryExportFormat {
    Json,
    Markdown,
}

impl HistoryExportFormat {
    /// What the save dialog came back with decides the format, so an unknown or
    /// missing extension has to mean something. JSON is the lossless one: it
    /// distinguishes an entry that was never cleaned from one cleaned to the
    /// same text, which Markdown cannot.
    pub fn for_path(path: &str) -> Self {
        match path.rsplit('.').next().map(str::to_ascii_lowercase) {
            Some(extension) if extension == "md" || extension == "markdown" => Self::Markdown,
            _ => Self::Json,
        }
    }
}

/// One entry as the JSON file carries it.
///
/// A separate type from `HistoryEntry` on purpose: this one is a published file
/// format that someone else's script will parse, and pinning it here means a
/// column added to the database later cannot silently change the shape of
/// everyone's exports.
#[derive(Debug, Serialize)]
struct ExportedEntry {
    id: i64,
    /// So an export can be matched against the recordings folder.
    audio_file: String,
    recorded_at: String,
    title: String,
    saved: bool,
    transcript: String,
    /// Absent entirely when cleanup is off. Present and null when cleanup is on
    /// but this entry has none, which is the distinction a consumer needs.
    #[serde(skip_serializing_if = "Option::is_none")]
    cleaned: Option<Option<String>>,
}

#[derive(Debug, Serialize)]
struct ExportedHistory {
    exported_at: String,
    entry_count: usize,
    /// False means cleanup was switched off when this was written, so the
    /// absence of cleaned text says nothing about the entries themselves.
    includes_cleaned_text: bool,
    entries: Vec<ExportedEntry>,
}

/// The cleaned text to export for one entry, or `None` when there is nothing to
/// say about cleanup.
///
/// Blank counts as nothing. A provider answering 200 with an empty body reaches
/// the database as `Some("")`, and an export claiming that entry was cleaned to
/// an empty string would be a worse record than one that stays quiet.
fn cleaned_text(entry: &HistoryEntry, include: bool) -> Option<&str> {
    if !include {
        return None;
    }
    entry
        .post_processed_text
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
}

fn local_timestamp(timestamp: i64) -> String {
    DateTime::from_timestamp(timestamp, 0).map_or_else(
        || timestamp.to_string(),
        |utc| {
            utc.with_timezone(&Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        },
    )
}

fn iso_timestamp(timestamp: i64) -> String {
    DateTime::from_timestamp(timestamp, 0).map_or_else(
        || timestamp.to_string(),
        |utc| utc.to_rfc3339_opts(SecondsFormat::Secs, true),
    )
}

pub fn to_json(entries: &[HistoryEntry], include_cleaned: bool) -> Result<String, String> {
    let document = ExportedHistory {
        exported_at: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        entry_count: entries.len(),
        includes_cleaned_text: include_cleaned,
        entries: entries
            .iter()
            .map(|entry| ExportedEntry {
                id: entry.id,
                audio_file: entry.file_name.clone(),
                recorded_at: iso_timestamp(entry.timestamp),
                title: entry.title.clone(),
                saved: entry.saved,
                transcript: entry.transcription_text.clone(),
                cleaned: include_cleaned.then(|| cleaned_text(entry, true).map(str::to_string)),
            })
            .collect(),
    };

    serde_json::to_string_pretty(&document).map_err(|e| format!("Failed to build the export: {e}"))
}

/// Keep a transcript from becoming document structure.
///
/// A dictation is arbitrary text: one that opens with "hash" transcribed as `#`,
/// or quotes something with `>`, would otherwise turn into a heading or a
/// blockquote and reshape the file around it. Escaping only at the start of a
/// line is enough, because that is the only place these characters mean
/// anything in Markdown, and it leaves prose untouched everywhere else.
fn escape_block(text: &str) -> String {
    text.lines()
        .map(|line| {
            let trimmed = line.trim_start();
            if trimmed.starts_with(['#', '>', '-', '+', '|'])
                || trimmed.starts_with("```")
                || trimmed.starts_with("~~~")
            {
                let indent = &line[..line.len() - trimmed.len()];
                format!("{indent}\\{trimmed}")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn to_markdown(entries: &[HistoryEntry], include_cleaned: bool) -> String {
    let mut out = String::from("# Handy history\n\n");
    out.push_str(&format!(
        "Exported {} - {} {}\n",
        local_timestamp(Utc::now().timestamp()),
        entries.len(),
        if entries.len() == 1 {
            "entry"
        } else {
            "entries"
        }
    ));

    for entry in entries {
        out.push_str("\n## ");
        out.push_str(&local_timestamp(entry.timestamp));
        let title = entry.title.trim();
        if !title.is_empty() {
            out.push_str(" - ");
            // On the heading line, so a newline in a title would end the
            // heading early and leave the rest as loose text.
            out.push_str(&title.replace(['\n', '\r'], " "));
        }
        out.push_str("\n\n**Transcript**\n\n");
        out.push_str(&escape_block(entry.transcription_text.trim()));
        out.push('\n');

        if let Some(cleaned) = cleaned_text(entry, include_cleaned) {
            out.push_str("\n**Cleaned**\n\n");
            out.push_str(&escape_block(cleaned));
            out.push('\n');
        }
    }

    out
}

/// Write the whole history to `path`.
///
/// The caller has already chosen the path through the platform's save dialog,
/// which is also where an overwrite was confirmed.
#[tauri::command]
#[specta::specta]
pub async fn export_history(
    app: AppHandle,
    history_manager: State<'_, Arc<HistoryManager>>,
    path: String,
    format: HistoryExportFormat,
) -> Result<usize, String> {
    let page = history_manager
        .get_history_entries(None, None)
        .await
        .map_err(|e| format!("Failed to read history: {e}"))?;
    let entries = page.entries;

    // The same flag that shows or hides the page's Processed tab, so a file
    // carries what the page was showing when it was asked for.
    let include_cleaned = crate::settings::get_settings(&app).post_process_enabled;

    let contents = match format {
        HistoryExportFormat::Json => to_json(&entries, include_cleaned)?,
        HistoryExportFormat::Markdown => to_markdown(&entries, include_cleaned),
    };

    // Written whole. A partially written file that still parses is the one
    // failure mode worth engineering against here, because the user would keep
    // it believing it was their history.
    std::fs::write(&path, contents).map_err(|e| format!("Failed to write the export: {e}"))?;

    log::info!(
        "Exported {} history entries ({} cleaned text)",
        entries.len(),
        if include_cleaned { "with" } else { "without" }
    );
    Ok(entries.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: i64, transcript: &str, cleaned: Option<&str>) -> HistoryEntry {
        HistoryEntry {
            id,
            file_name: format!("handy-{id}.wav"),
            // 2026-09-20T16:02:23Z
            timestamp: 1_789_920_143,
            saved: false,
            title: format!("Entry {id}"),
            transcription_text: transcript.to_string(),
            post_processed_text: cleaned.map(str::to_string),
            post_process_prompt: None,
            post_process_requested: cleaned.is_some(),
        }
    }

    #[test]
    fn cleanup_switched_off_exports_the_transcript_alone() {
        let entries = vec![entry(1, "raw words", Some("Raw words."))];

        let markdown = to_markdown(&entries, false);
        assert!(markdown.contains("raw words"));
        assert!(
            !markdown.contains("Raw words."),
            "cleaned text leaked with cleanup off:\n{markdown}"
        );
        assert!(!markdown.contains("**Cleaned**"));

        let json: serde_json::Value = serde_json::from_str(&to_json(&entries, false).unwrap())
            .expect("the export must parse");
        assert_eq!(json["includes_cleaned_text"], false);
        assert!(
            json["entries"][0].get("cleaned").is_none(),
            "the key must be absent, not null, so it cannot read as 'never cleaned'"
        );
    }

    #[test]
    fn an_entry_without_cleaned_text_says_nothing_about_cleanup() {
        // Cleanup is on, but this entry has none, and one has an empty string
        // from a provider that answered 200 with an empty body.
        let entries = vec![
            entry(2, "just the transcript", None),
            entry(3, "another", Some("   ")),
        ];

        let markdown = to_markdown(&entries, true);
        assert!(
            !markdown.contains("**Cleaned**"),
            "an empty cleanup must not render a section:\n{markdown}"
        );

        let json: serde_json::Value =
            serde_json::from_str(&to_json(&entries, true).unwrap()).unwrap();
        assert_eq!(json["includes_cleaned_text"], true);
        // Present and null: cleanup was on and this entry genuinely has none.
        assert!(json["entries"][0]["cleaned"].is_null());
        assert!(json["entries"][1]["cleaned"].is_null());
    }

    #[test]
    fn both_texts_are_exported_when_there_are_both() {
        let entries = vec![entry(
            4,
            "um the deploy looks fine",
            Some("The deploy looks fine."),
        )];

        let markdown = to_markdown(&entries, true);
        let transcript_at = markdown.find("**Transcript**").unwrap();
        let cleaned_at = markdown.find("**Cleaned**").unwrap();
        assert!(transcript_at < cleaned_at, "transcript comes first");
        assert!(markdown.contains("um the deploy looks fine"));
        assert!(markdown.contains("The deploy looks fine."));

        let json: serde_json::Value =
            serde_json::from_str(&to_json(&entries, true).unwrap()).unwrap();
        assert_eq!(json["entries"][0]["transcript"], "um the deploy looks fine");
        assert_eq!(json["entries"][0]["cleaned"], "The deploy looks fine.");
        assert_eq!(json["entries"][0]["audio_file"], "handy-4.wav");
        assert_eq!(json["entries"][0]["recorded_at"], "2026-09-20T16:02:23Z");
    }

    #[test]
    fn a_dictation_cannot_restructure_the_markdown_around_it() {
        // Whisper writes what it hears, and "hash" or a quoted line arrives as
        // exactly these characters at the start of a line.
        let entries = vec![entry(
            5,
            "# not a heading\n> not a quote\n- not a list\nordinary prose # stays",
            None,
        )];

        let markdown = to_markdown(&entries, true);
        let body = markdown.split("**Transcript**").nth(1).unwrap();
        assert!(body.contains("\\# not a heading"));
        assert!(body.contains("\\> not a quote"));
        assert!(body.contains("\\- not a list"));
        // Only the line start matters, so prose is left alone.
        assert!(body.contains("ordinary prose # stays"));

        // The heading count is the structural claim: one document heading and
        // one entry heading, whatever the dictation said.
        assert_eq!(markdown.lines().filter(|l| l.starts_with("## ")).count(), 1);
    }

    #[test]
    fn a_newline_in_a_title_does_not_end_the_heading_early() {
        let mut broken = entry(6, "text", None);
        broken.title = "first line\nsecond line".to_string();

        let markdown = to_markdown(&[broken], false);
        let heading = markdown
            .lines()
            .find(|line| line.starts_with("## "))
            .unwrap();
        assert!(heading.contains("first line second line"));
    }

    #[test]
    fn an_empty_history_still_produces_a_usable_file() {
        let json: serde_json::Value = serde_json::from_str(&to_json(&[], true).unwrap()).unwrap();
        assert_eq!(json["entry_count"], 0);
        assert_eq!(json["entries"].as_array().unwrap().len(), 0);

        let markdown = to_markdown(&[], true);
        assert!(markdown.starts_with("# Handy history"));
        assert!(markdown.contains("0 entries"));
    }

    #[test]
    fn the_format_follows_the_extension_the_dialog_returned() {
        assert_eq!(
            HistoryExportFormat::for_path("C:\\Users\\me\\history.md"),
            HistoryExportFormat::Markdown
        );
        assert_eq!(
            HistoryExportFormat::for_path("/home/me/history.MARKDOWN"),
            HistoryExportFormat::Markdown
        );
        assert_eq!(
            HistoryExportFormat::for_path("/home/me/history.json"),
            HistoryExportFormat::Json
        );
        // No extension, or one nobody asked for: JSON, because it is the format
        // that can represent everything.
        assert_eq!(
            HistoryExportFormat::for_path("/home/me/history"),
            HistoryExportFormat::Json
        );
    }
}
