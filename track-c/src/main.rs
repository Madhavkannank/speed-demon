use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::Path;

struct Country {
    count: u64,
    id_sum: i64,
    amount_sum: i64,
    amount_min: Option<i64>,
    amount_max: Option<i64>,
    names: HashSet<String>,
    name_bytes: usize,
    note_bytes: usize,
}

impl Country {
    fn new() -> Self {
        Self {
            count: 0,
            id_sum: 0,
            amount_sum: 0,
            amount_min: None,
            amount_max: None,
            names: HashSet::new(),
            name_bytes: 0,
            note_bytes: 0,
        }
    }

    fn add(&mut self, record_id: i64, name: String, note_len: usize, cents: i64) {
        self.count += 1;
        self.id_sum += record_id;
        self.amount_sum += cents;
        self.amount_min = Some(match self.amount_min {
            Some(m) => m.min(cents),
            None => cents,
        });
        self.amount_max = Some(match self.amount_max {
            Some(m) => m.max(cents),
            None => cents,
        });
        self.name_bytes += name.len();
        self.names.insert(name);
        self.note_bytes += note_len;
    }
}

fn parse_cents(s: &str) -> i64 {
    let (neg, rest) = if let Some(stripped) = s.strip_prefix('-') {
        (true, stripped)
    } else {
        (false, s)
    };
    let dot = rest.find('.').expect("no dot in amount");
    let whole: i64 = rest[..dot].parse().expect("bad whole");
    let frac: i64 = rest[dot + 1..].parse().expect("bad frac");
    let cents = whole * 100 + frac;
    if neg {
        -cents
    } else {
        cents
    }
}

fn format_cents(cents: i64) -> String {
    let sign = if cents < 0 { "-" } else { "" };
    let c = cents.abs();
    format!("{}{}.{:02}", sign, c / 100, c % 100)
}

fn load_regions(path: &Path) -> std::io::Result<HashMap<u32, String>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut map = HashMap::new();
    let mut lines = reader.lines();

    // Skip header region_id,country
    let _header = lines.next();

    for line_res in lines {
        let line = line_res?;
        if line.is_empty() {
            continue;
        }
        if let Some((id_str, country)) = line.split_once(',') {
            if let Ok(id) = id_str.trim().parse::<u32>() {
                map.insert(id, country.trim().to_string());
            }
        }
    }
    Ok(map)
}

#[allow(unused_assignments)]
fn main() -> std::io::Result<()> {
    let data_dir = std::env::var("DATA_DIR").unwrap_or_else(|_| "/data".to_string());
    let out_dir = std::env::var("OUT_DIR").unwrap_or_else(|_| "/out".to_string());

    let regions_path = Path::new(&data_dir).join("regions.csv");
    let records_path = Path::new(&data_dir).join("records.csv");
    let out_path = Path::new(&out_dir).join("aggregates.txt");

    let regions = load_regions(&regions_path)?;

    let records_file = File::open(&records_path)?;
    let mut reader = BufReader::with_capacity(512 * 1024, records_file);

    let mut countries: HashMap<String, Country> = HashMap::new();
    let unmatched = "_UNMATCHED".to_string();

    // Streaming RFC 4180 CSV state machine
    let mut buf = [0u8; 64 * 1024];
    let mut fields: Vec<Vec<u8>> = Vec::with_capacity(5);
    let mut cur_field: Vec<u8> = Vec::with_capacity(128);

    enum State {
        StartField,
        InUnquoted,
        InQuoted,
        AfterQuote,
    }

    let mut state = State::StartField;
    let mut is_header = true;

    macro_rules! finish_row {
        () => {
            if !fields.is_empty() || !cur_field.is_empty() {
                fields.push(std::mem::take(&mut cur_field));
                if fields.len() == 5 {
                    // Strip BOM if present on field 0
                    if fields[0].starts_with(&[0xEF, 0xBB, 0xBF]) {
                        fields[0].drain(0..3);
                    }

                    if is_header {
                        is_header = false;
                    } else {
                        let id_str = std::str::from_utf8(&fields[0]).unwrap();
                        let name_str = std::str::from_utf8(&fields[1]).unwrap().to_string();
                        let note_bytes_len = fields[2].len();
                        let reg_str = std::str::from_utf8(&fields[3]).unwrap();
                        let amt_str = std::str::from_utf8(&fields[4]).unwrap();

                        let id: i64 = id_str.parse().unwrap();
                        let reg_id: u32 = reg_str.parse().unwrap();
                        let cents = parse_cents(amt_str);

                        let country_key = regions.get(&reg_id).unwrap_or(&unmatched);
                        let country = countries.entry(country_key.clone()).or_insert_with(Country::new);
                        country.add(id, name_str, note_bytes_len, cents);
                    }
                }
                fields.clear();
            }
        };
    }

    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            finish_row!();
            break;
        }

        for &b in &buf[..n] {
            match state {
                State::StartField => match b {
                    b'"' => state = State::InQuoted,
                    b',' => {
                        fields.push(std::mem::take(&mut cur_field));
                    }
                    b'\r' => {}
                    b'\n' => {
                        finish_row!();
                        state = State::StartField;
                    }
                    _ => {
                        cur_field.push(b);
                        state = State::InUnquoted;
                    }
                },
                State::InUnquoted => match b {
                    b',' => {
                        fields.push(std::mem::take(&mut cur_field));
                        state = State::StartField;
                    }
                    b'\r' => {}
                    b'\n' => {
                        finish_row!();
                        state = State::StartField;
                    }
                    _ => cur_field.push(b),
                },
                State::InQuoted => match b {
                    b'"' => state = State::AfterQuote,
                    _ => cur_field.push(b),
                },
                State::AfterQuote => match b {
                    b'"' => {
                        cur_field.push(b'"');
                        state = State::InQuoted;
                    }
                    b',' => {
                        fields.push(std::mem::take(&mut cur_field));
                        state = State::StartField;
                    }
                    b'\r' => {}
                    b'\n' => {
                        finish_row!();
                        state = State::StartField;
                    }
                    _ => {
                        cur_field.push(b);
                    }
                },
            }
        }
    }

    fs::create_dir_all(&out_dir)?;
    let out_file = File::create(&out_path)?;
    let mut writer = BufWriter::with_capacity(512 * 1024, out_file);

    let mut keys: Vec<String> = countries.keys().cloned().collect();
    keys.sort_unstable();

    for key in keys {
        let c = &countries[&key];
        writeln!(
            writer,
            "{} {} {} {} {} {} {} {} {}",
            key,
            c.count,
            c.id_sum,
            format_cents(c.amount_sum),
            format_cents(c.amount_min.unwrap_or(0)),
            format_cents(c.amount_max.unwrap_or(0)),
            c.names.len(),
            c.name_bytes,
            c.note_bytes
        )?;
    }

    writer.flush()?;
    Ok(())
}
