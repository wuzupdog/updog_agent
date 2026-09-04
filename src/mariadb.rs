use chrono::{DateTime, SecondsFormat, Utc};
use ring::digest::{digest, SHA256};
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

const MAX_READ_BYTES: u64 = 1024 * 1024;
const MAX_PENDING_BYTES: usize = 256 * 1024;
#[derive(Debug, PartialEq)]
pub struct SlowQuery {
    pub query_time_seconds: f64,
    pub lock_time_seconds: f64,
    pub rows_sent: u64,
    pub rows_examined: u64,
    pub fingerprint: String,
    pub recorded_at: Option<String>,
}

pub struct SlowLogCollector {
    path: PathBuf,
    file_identity: Option<(u64, u64)>,
    seen_file: bool,
    offset: u64,
    pending: String,
}

impl SlowLogCollector {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let metadata = fs::metadata(&path).ok();

        Self {
            file_identity: metadata.as_ref().map(file_identity),
            seen_file: metadata.is_some(),
            offset: metadata.as_ref().map_or(0, |metadata| metadata.len()),
            path,
            pending: String::new(),
        }
    }

    pub fn poll(&mut self) -> io::Result<Vec<SlowQuery>> {
        let metadata = match fs::metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.reset_missing_file();
                return Ok(Vec::new());
            }
            Err(error) => return Err(error),
        };
        let identity = file_identity(&metadata);

        if self.file_identity.is_none() {
            self.file_identity = Some(identity);
            if self.seen_file {
                self.offset = 0;
            } else {
                self.seen_file = true;
                self.offset = metadata.len();
                return Ok(Vec::new());
            }
        }

        if self.file_identity != Some(identity) || metadata.len() < self.offset {
            self.file_identity = Some(identity);
            self.offset = 0;
            self.pending.clear();
        }

        let mut file = File::open(&self.path)?;
        file.seek(SeekFrom::Start(self.offset))?;
        let mut bytes = Vec::new();
        file.take(MAX_READ_BYTES).read_to_end(&mut bytes)?;
        self.offset += bytes.len() as u64;
        self.pending.push_str(&String::from_utf8_lossy(&bytes));
        bound_pending(&mut self.pending);

        Ok(take_complete_entries(&mut self.pending)
            .into_iter()
            .filter_map(parse_entry)
            .collect())
    }

    fn reset_missing_file(&mut self) {
        self.file_identity = None;
        self.offset = 0;
        self.pending.clear();
    }

    #[cfg(test)]
    fn from_start(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let metadata = fs::metadata(&path).ok();

        Self {
            path,
            file_identity: metadata.as_ref().map(file_identity),
            seen_file: true,
            offset: 0,
            pending: String::new(),
        }
    }
}

fn file_identity(metadata: &fs::Metadata) -> (u64, u64) {
    (metadata.dev(), metadata.ino())
}

fn bound_pending(pending: &mut String) {
    if pending.len() <= MAX_PENDING_BYTES {
        return;
    }

    let mut minimum = pending.len() - MAX_PENDING_BYTES;
    while !pending.is_char_boundary(minimum) {
        minimum += 1;
    }
    let start = pending[minimum..]
        .find("# Time:")
        .map_or(pending.len(), |position| minimum + position);
    pending.drain(..start);
}

fn take_complete_entries(pending: &mut String) -> Vec<String> {
    let starts = entry_starts(pending);
    if starts.is_empty() {
        return Vec::new();
    }

    if starts[0] > 0 {
        pending.drain(..starts[0]);
    }

    let starts = entry_starts(pending);
    let mut consumed = 0;
    let mut entries = Vec::new();

    for window in starts.windows(2) {
        entries.push(pending[window[0]..window[1]].to_string());
        consumed = window[1];
    }

    if let Some(last_start) = starts.last().copied() {
        let final_entry = &pending[last_start..];
        if is_complete_entry(final_entry) {
            entries.push(final_entry.to_string());
            consumed = pending.len();
        }
    }

    pending.drain(..consumed);
    entries
}

fn entry_starts(contents: &str) -> Vec<usize> {
    contents
        .match_indices("# Time:")
        .filter_map(|(position, _)| {
            (position == 0 || contents.as_bytes().get(position.wrapping_sub(1)) == Some(&b'\n'))
                .then_some(position)
        })
        .collect()
}

fn is_complete_entry(entry: &str) -> bool {
    entry.ends_with(";\n") || entry.ends_with(";\r\n")
}

fn parse_entry(entry: String) -> Option<SlowQuery> {
    let query_time_seconds = metadata_number(&entry, "Query_time:")?;
    let lock_time_seconds = metadata_number(&entry, "Lock_time:").unwrap_or_default();
    let rows_sent = metadata_integer(&entry, "Rows_sent:").unwrap_or_default();
    let rows_examined = metadata_integer(&entry, "Rows_examined:").unwrap_or_default();
    let recorded_at = unix_timestamp(&entry);
    let statement = sql_statement(&entry);
    let normalized = normalize_sql(&statement);

    if normalized.is_empty() {
        return None;
    }

    Some(SlowQuery {
        query_time_seconds,
        lock_time_seconds,
        rows_sent,
        rows_examined,
        fingerprint: fingerprint(&normalized),
        recorded_at,
    })
}

fn metadata_number(entry: &str, key: &str) -> Option<f64> {
    metadata_value(entry, key)?.parse().ok()
}

fn metadata_integer(entry: &str, key: &str) -> Option<u64> {
    metadata_value(entry, key)?.parse().ok()
}

fn metadata_value<'a>(entry: &'a str, key: &str) -> Option<&'a str> {
    entry.lines().find_map(|line| {
        let (_, rest) = line.split_once(key)?;
        rest.split_whitespace().next()
    })
}

fn unix_timestamp(entry: &str) -> Option<String> {
    let timestamp = entry.lines().find_map(|line| {
        line.trim()
            .strip_prefix("SET timestamp=")?
            .trim_end_matches(';')
            .parse::<i64>()
            .ok()
    })?;

    DateTime::<Utc>::from_timestamp(timestamp, 0)
        .map(|datetime| datetime.to_rfc3339_opts(SecondsFormat::Secs, true))
}

fn sql_statement(entry: &str) -> String {
    entry
        .lines()
        .filter(|line| {
            let line = line.trim_start();
            !line.starts_with('#')
                && !line.starts_with("SET timestamp=")
                && !line.starts_with("use ")
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim_end_matches(';')
        .to_string()
}

fn normalize_sql(statement: &str) -> String {
    let characters = statement.chars().collect::<Vec<_>>();
    let mut normalized = String::with_capacity(statement.len());
    let mut index = 0;

    while index < characters.len() {
        let character = characters[index];

        if character == '\'' || character == '"' {
            index = skip_quoted_literal(&characters, index, character);
            push_token(&mut normalized, "?");
        } else if starts_line_comment(&characters, index) {
            index = skip_line_comment(&characters, index + 2);
        } else if starts_block_comment(&characters, index) {
            index = skip_block_comment(&characters, index + 2);
        } else if numeric_literal_start(&characters, index) {
            index = skip_numeric_literal(&characters, index);
            push_token(&mut normalized, "?");
        } else if character.is_whitespace() {
            push_space(&mut normalized);
            index += 1;
        } else {
            normalized.push(character.to_ascii_lowercase());
            index += 1;
        }
    }

    normalized.trim().to_string()
}

fn skip_quoted_literal(characters: &[char], mut index: usize, quote: char) -> usize {
    index += 1;
    while index < characters.len() {
        if characters[index] == '\\' {
            index = (index + 2).min(characters.len());
        } else if characters[index] == quote {
            if characters.get(index + 1) == Some(&quote) {
                index += 2;
            } else {
                return index + 1;
            }
        } else {
            index += 1;
        }
    }
    index
}

fn starts_line_comment(characters: &[char], index: usize) -> bool {
    characters.get(index) == Some(&'-') && characters.get(index + 1) == Some(&'-')
}

fn skip_line_comment(characters: &[char], mut index: usize) -> usize {
    while index < characters.len() && characters[index] != '\n' {
        index += 1;
    }
    index
}

fn starts_block_comment(characters: &[char], index: usize) -> bool {
    characters.get(index) == Some(&'/') && characters.get(index + 1) == Some(&'*')
}

fn skip_block_comment(characters: &[char], mut index: usize) -> usize {
    while index + 1 < characters.len() {
        if characters[index] == '*' && characters[index + 1] == '/' {
            return index + 2;
        }
        index += 1;
    }
    characters.len()
}

fn numeric_literal_start(characters: &[char], index: usize) -> bool {
    characters[index].is_ascii_digit()
        && index
            .checked_sub(1)
            .and_then(|previous| characters.get(previous))
            .is_none_or(|character| !is_identifier_character(*character))
}

fn skip_numeric_literal(characters: &[char], mut index: usize) -> usize {
    while index < characters.len()
        && (characters[index].is_ascii_alphanumeric()
            || matches!(characters[index], '.' | '+' | '-' | '_'))
    {
        index += 1;
    }
    index
}

fn is_identifier_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '_' | '$')
}

fn push_token(output: &mut String, token: &str) {
    if output.ends_with('?') && token == "?" {
        return;
    }
    output.push_str(token);
}

fn push_space(output: &mut String) {
    if !output.is_empty() && !output.ends_with(' ') {
        output.push(' ');
    }
}

fn fingerprint(query: &str) -> String {
    let mut fingerprint = String::with_capacity(32);
    for byte in digest(&SHA256, query.as_bytes()).as_ref().iter().take(16) {
        fingerprint.push_str(&format!("{byte:02x}"));
    }
    fingerprint
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::time::{SystemTime, UNIX_EPOCH};

    const ENTRY: &str = "# Time: 2026-09-03T17:34:30.123456Z\n\
# User@Host: game[game] @ localhost []\n\
# Thread_id: 42  Schema: game  QC_hit: No\n\
# Query_time: 2.750000  Lock_time: 0.125000  Rows_sent: 3  Rows_examined: 48000\n\
SET timestamp=1788456870;\n\
SELECT * FROM players WHERE email = 'secret@example.com' AND level > 42;\n";

    #[test]
    fn parses_and_redacts_slow_query_entries() {
        let query = parse_entry(ENTRY.to_string()).unwrap();

        assert_eq!(query.query_time_seconds, 2.75);
        assert_eq!(query.lock_time_seconds, 0.125);
        assert_eq!(query.rows_sent, 3);
        assert_eq!(query.rows_examined, 48_000);
        assert_eq!(query.fingerprint.len(), 32);
        assert_eq!(query.recorded_at.as_deref(), Some("2026-09-03T17:34:30Z"));
    }

    #[test]
    fn fingerprints_queries_independently_of_literal_values() {
        let first = normalize_sql("SELECT * FROM players WHERE id = 42 AND name = 'one'");
        let second = normalize_sql("select * from players where id = 99 and name = 'two'");

        assert_eq!(first, second);
        assert_eq!(fingerprint(&first), fingerprint(&second));
    }

    #[test]
    fn collector_starts_at_eof_and_reads_only_new_entries() {
        let path = temporary_log_path();
        fs::write(&path, ENTRY).unwrap();
        let mut collector = SlowLogCollector::new(&path);

        assert!(collector.poll().unwrap().is_empty());

        File::options()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(ENTRY.as_bytes())
            .unwrap();

        assert_eq!(collector.poll().unwrap().len(), 1);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn collector_reads_from_start_for_test_fixtures() {
        let path = temporary_log_path();
        fs::write(&path, ENTRY).unwrap();
        let mut collector = SlowLogCollector::from_start(&path);

        assert_eq!(collector.poll().unwrap().len(), 1);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn collector_follows_log_replacement() {
        let path = temporary_log_path();
        let rotated_path = path.with_extension("log.1");
        fs::write(&path, ENTRY).unwrap();
        let mut collector = SlowLogCollector::new(&path);

        fs::rename(&path, &rotated_path).unwrap();
        fs::write(&path, ENTRY).unwrap();

        assert_eq!(collector.poll().unwrap().len(), 1);
        fs::remove_file(path).unwrap();
        fs::remove_file(rotated_path).unwrap();
    }

    #[test]
    fn collector_reads_a_replacement_created_after_a_missing_poll() {
        let path = temporary_log_path();
        fs::write(&path, ENTRY).unwrap();
        let mut collector = SlowLogCollector::new(&path);

        fs::remove_file(&path).unwrap();
        assert!(collector.poll().unwrap().is_empty());
        fs::write(&path, ENTRY).unwrap();

        assert_eq!(collector.poll().unwrap().len(), 1);
        fs::remove_file(path).unwrap();
    }

    fn temporary_log_path() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("updog-mariadb-{unique}.log"))
    }
}
