//! Parser for the LRC lyrics format used by lrclib.net.

use super::LyricsLine;

/// Parse LRC text into lines sorted by start time.
///
/// Supports multiple timestamps per line (`[00:12.00][01:50.32]text`) and
/// ignores metadata tags like `[ar:...]`.
pub fn parse(lrc: &str) -> Vec<LyricsLine> {
    let mut lines = Vec::new();

    for raw in lrc.lines() {
        let mut rest = raw.trim();
        let mut times = Vec::new();

        while let Some(stripped) = rest.strip_prefix('[') {
            let Some((tag, after)) = stripped.split_once(']') else {
                break;
            };
            match parse_timestamp(tag) {
                Some(time_ms) => times.push(time_ms),
                // A non-timestamp tag (metadata like [ar:...]) ends the tag run.
                None => break,
            }
            rest = after.trim_start();
        }

        let text = rest.trim().to_string();
        for time_ms in times {
            lines.push(LyricsLine {
                time_ms,
                text: text.clone(),
            });
        }
    }

    lines.sort_by_key(|line| line.time_ms);
    lines
}

/// Parse a `mm:ss.xx` (or `mm:ss`) timestamp into milliseconds.
fn parse_timestamp(tag: &str) -> Option<u32> {
    let (minutes, seconds) = tag.split_once(':')?;
    let minutes: u32 = minutes.trim().parse().ok()?;
    let seconds: f64 = seconds.trim().parse().ok()?;
    if !(0.0..60.0).contains(&seconds) {
        return None;
    }
    Some(minutes * 60_000 + (seconds * 1000.0) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_lines() {
        let lrc = "[00:12.00]Line one\n[00:17.20]Line two";
        let lines = parse(lrc);
        assert_eq!(
            lines,
            vec![
                LyricsLine {
                    time_ms: 12_000,
                    text: "Line one".into()
                },
                LyricsLine {
                    time_ms: 17_200,
                    text: "Line two".into()
                },
            ]
        );
    }

    #[test]
    fn parses_multiple_timestamps_per_line() {
        let lines = parse("[00:50.00][00:10.00]Chorus");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].time_ms, 10_000);
        assert_eq!(lines[1].time_ms, 50_000);
        assert!(lines.iter().all(|l| l.text == "Chorus"));
    }

    #[test]
    fn ignores_metadata_tags() {
        let lrc = "[ar:Some Artist]\n[ti:Some Title]\n[00:01.50]Hello";
        let lines = parse(lrc);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].time_ms, 1_500);
        assert_eq!(lines[0].text, "Hello");
    }

    #[test]
    fn keeps_empty_lines_for_pauses() {
        let lines = parse("[00:05.00]\n[00:10.00]After the pause");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].text, "");
    }

    #[test]
    fn sorts_by_time() {
        let lines = parse("[01:00.00]B\n[00:30.00]A");
        assert_eq!(lines[0].text, "A");
        assert_eq!(lines[1].text, "B");
    }

    #[test]
    fn parses_timestamp_without_fraction() {
        assert_eq!(parse_timestamp("02:03"), Some(123_000));
        assert_eq!(parse_timestamp("xx:03"), None);
        assert_eq!(parse_timestamp("02:73"), None);
    }
}
