//! Read-only usage inspection: `otto storage window-peaks`.

use std::io::Write;
use std::path::Path;

use otto::storage::Connection;

pub fn run(args: &[String], stdout: &mut dyn Write, stderr: &mut dyn Write) -> i32 {
    let result = match args {
        [command, database, session] if command == "window-peaks" => {
            window_peaks(Path::new(database), session, stdout)
        }
        _ => {
            let _ = writeln!(
                stderr,
                "usage: otto storage window-peaks <usage.db> <session-id>"
            );
            return 2;
        }
    };
    match result {
        Ok(()) => 0,
        Err(()) => {
            let _ = writeln!(stderr, "storage operation failed");
            1
        }
    }
}

fn window_peaks(path: &Path, session: &str, stdout: &mut dyn Write) -> Result<(), ()> {
    let connection = Connection::open_read_only(path).map_err(|_| ())?;
    let mut statement = connection
        .prepare(
            "SELECT task_id, MAX(input_tokens), COUNT(*) FROM usage_events \
         WHERE session_id=? AND usage_present=1 GROUP BY task_id ORDER BY (task_id<>''),task_id",
        )
        .map_err(|_| ())?;
    let rows = statement
        .query_map([session], |row| {
            Ok((
                row.get::<String>(0)?,
                row.get::<i64>(1)?,
                row.get::<i64>(2)?,
            ))
        })
        .map_err(|_| ())?;
    let mut peaks = Vec::new();
    for row in rows {
        let (task, peak, requests) = row.map_err(|_| ())?;
        peaks.push(serde_json::json!({"window": if task.is_empty() { "main" } else { &task }, "peakInput": peak, "requests": requests}));
    }
    serde_json::to_writer(&mut *stdout, &peaks).map_err(|_| ())?;
    writeln!(stdout).map_err(|_| ())
}
