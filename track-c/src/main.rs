use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::Path;

struct CountryAgg {
    count: u64,
    id_sum: i64,
    amount_sum: i64,
    amount_min: i64,
    amount_max: i64,
    name_bytes: usize,
    note_bytes: usize,
    names: HashSet<u32>,
}

impl CountryAgg {
    fn new() -> Self {
        Self {
            count: 0,
            id_sum: 0,
            amount_sum: 0,
            amount_min: i64::MAX,
            amount_max: i64::MIN,
            name_bytes: 0,
            note_bytes: 0,
            names: HashSet::new(),
        }
    }

    #[inline(always)]
    fn add(&mut self, record_id: i64, name_id: u32, name_len: usize, note_len: usize, cents: i64) {
        self.count += 1;
        self.id_sum += record_id;
        self.amount_sum += cents;
        if cents < self.amount_min {
            self.amount_min = cents;
        }
        if cents > self.amount_max {
            self.amount_max = cents;
        }
        self.name_bytes += name_len;
        self.note_bytes += note_len;
        self.names.insert(name_id);
    }
}

#[inline(always)]
fn parse_id(bytes: &[u8]) -> i64 {
    let mut val = 0i64;
    for &b in bytes {
        if b.is_ascii_digit() {
            val = val * 10 + (b - b'0') as i64;
        }
    }
    val
}

#[inline(always)]
fn parse_region_id(bytes: &[u8]) -> u32 {
    let mut val = 0u32;
    for &b in bytes {
        if b.is_ascii_digit() {
            val = val * 10 + (b - b'0') as u32;
        }
    }
    val
}

#[inline(always)]
fn parse_cents(s: &[u8]) -> i64 {
    let (neg, rest) = if !s.is_empty() && s[0] == b'-' {
        (true, &s[1..])
    } else {
        (false, s)
    };
    let dot = rest.iter().position(|&b| b == b'.').unwrap_or(rest.len());
    let mut whole = 0i64;
    for &b in &rest[..dot] {
        if b.is_ascii_digit() {
            whole = whole * 10 + (b - b'0') as i64;
        }
    }
    let frac = if dot + 2 < rest.len() {
        (rest[dot + 1] - b'0') as i64 * 10 + (rest[dot + 2] - b'0') as i64
    } else {
        0
    };
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

fn load_regions(
    path: &Path,
) -> std::io::Result<(Vec<u16>, Vec<String>)> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut lines = reader.lines();

    // Skip header region_id,country
    let _header = lines.next();

    let mut country_names = vec!["_UNMATCHED".to_string()];
    let mut country_to_id: HashMap<String, u16> = HashMap::new();
    country_to_id.insert("_UNMATCHED".to_string(), 0);

    let mut region_map = vec![0u16; 1_000_000];

    for line_res in lines {
        let line = line_res?;
        if line.is_empty() {
            continue;
        }
        if let Some((id_str, country)) = line.split_once(',') {
            let reg_id: usize = id_str.trim().parse().unwrap_or(0);
            let c_str = country.trim();
            let c_id = match country_to_id.get(c_str) {
                Some(&id) => id,
                None => {
                    let id = country_names.len() as u16;
                    country_names.push(c_str.to_string());
                    country_to_id.insert(c_str.to_string(), id);
                    id
                }
            };
            if reg_id < region_map.len() {
                region_map[reg_id] = c_id;
            }
        }
    }

    Ok((region_map, country_names))
}

#[allow(unused_assignments)]
fn main() -> std::io::Result<()> {
    let data_dir = std::env::var("DATA_DIR").unwrap_or_else(|_| "/data".to_string());
    let out_dir = std::env::var("OUT_DIR").unwrap_or_else(|_| "/out".to_string());

    let regions_path = Path::new(&data_dir).join("regions.csv");
    let records_path = Path::new(&data_dir).join("records.csv");
    let out_path = Path::new(&out_dir).join("aggregates.txt");

    let (region_map, country_names) = load_regions(&regions_path)?;

    let num_countries = country_names.len();
    let mut aggregates: Vec<CountryAgg> = (0..num_countries).map(|_| CountryAgg::new()).collect();
    let mut name_pool: HashMap<Box<[u8]>, u32> = HashMap::with_capacity(100_000);

    let records_file = File::open(&records_path)?;
    let mut reader = BufReader::with_capacity(1024 * 1024, records_file);

    let mut fields: Vec<Vec<u8>> = Vec::with_capacity(5);
    let mut cur_field = Vec::with_capacity(128);

    let mut buf = [0u8; 64 * 1024];

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
                        let id = parse_id(&fields[0]);
                        let reg_id = parse_region_id(&fields[3]);
                        let cents = parse_cents(&fields[4]);

                        let name_bytes = &fields[1];
                        let name_len = name_bytes.len();
                        let name_id = match name_pool.get(name_bytes.as_slice()) {
                            Some(&nid) => nid,
                            None => {
                                let boxed: Box<[u8]> = name_bytes.as_slice().into();
                                let nid = name_pool.len() as u32;
                                name_pool.insert(boxed, nid);
                                nid
                            }
                        };

                        let note_len = fields[2].len();

                        let country_id = if (reg_id as usize) < region_map.len() {
                            region_map[reg_id as usize] as usize
                        } else {
                            0
                        };

                        aggregates[country_id].add(id, name_id, name_len, note_len, cents);
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

    // Sort countries by name ascending (byte-order)
    let mut country_indices: Vec<usize> = (0..num_countries).collect();
    country_indices.sort_unstable_by(|&a, &b| country_names[a].cmp(&country_names[b]));

    fs::create_dir_all(&out_dir)?;
    let out_file = File::create(&out_path)?;
    let mut writer = BufWriter::with_capacity(1024 * 1024, out_file);

    for idx in country_indices {
        let agg = &aggregates[idx];
        if agg.count == 0 {
            continue;
        }
        let country_name = &country_names[idx];
        writeln!(
            writer,
            "{} {} {} {} {} {} {} {} {}",
            country_name,
            agg.count,
            agg.id_sum,
            format_cents(agg.amount_sum),
            format_cents(agg.amount_min),
            format_cents(agg.amount_max),
            agg.names.len(),
            agg.name_bytes,
            agg.note_bytes
        )?;
    }

    writer.flush()?;
    Ok(())
}
