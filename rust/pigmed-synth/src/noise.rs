//! Typing noise for recipe v3 (docs/RETRIEVER_QUERY_RECIPE_V3.md §5, stage C). It is applied in code from the
//! persona's habits instead of being asked of the LLM, whose typos look staged. Deterministic per (key, text).

use crate::persona::Persona;
use crate::sample::hash;

/// Common real-world misspellings of medical words (lower case; whole-word replacement).
const MISSPELL: &[(&str, &str)] = &[
    ("alzheimer's", "alzhimers"), ("alzheimer", "alzhimer"), ("metformin", "metforman"), ("diarrhea", "diarhea"),
    ("diarrhoea", "diarea"), ("pneumonia", "neumonia"), ("ibuprofen", "ibuprofin"), ("acetaminophen", "acetominophen"),
    ("paracetamol", "paracetemol"), ("schizophrenia", "skizophrenia"),
    ("arrhythmia", "arythmia"), ("hemorrhage", "hemorage"), ("haemorrhage", "hemmorhage"), ("leukemia", "leukaemia"),
    ("leukaemia", "lukemia"), ("cirrhosis", "cirosis"), ("rheumatoid", "rheumatiod"), ("osteoporosis", "osteoperosis"),
    ("ophthalmology", "opthalmology"), ("antibiotic", "antibiotc"), ("antibiotics", "anitbiotics"), ("vaccine", "vacine"),
    ("vaccination", "vacination"), ("diabetes", "diabeties"), ("cholesterol", "cholestrol"), ("hypertension", "hypertention"),
    ("anxiety", "anxeity"), ("seizure", "siezure"), ("seizures", "siezures"), ("prescription", "perscription"),
    ("ultrasound", "ultra sound"), ("psoriasis", "sorisis"), ("eczema", "exzema"), ("asthma", "asthama"),
    ("chemotherapy", "chemo therapy"), ("immunotherapy", "immuno therapy"), ("thyroid", "thyriod"), ("migraine", "migrane"),
    ("tylenol", "tylenal"), ("gabapentin", "gabapenten"), ("warfarin", "warfirin"), 
    ("colonoscopy", "colonscopy"), ("parkinson's", "parkinsons"), ("parkinson", "parkinsons"), ("sepsis", "sepsys"),
];

const ROWS: [&str; 3] = ["qwertyuiop", "asdfghjkl", "zxcvbnm"];

fn neighbour(c: char, x: u64) -> char {
    for (r, row) in ROWS.iter().enumerate() {
        if let Some(i) = row.find(c) {
            let mut opts: Vec<char> = vec![];
            if i > 0 { opts.push(row.as_bytes()[i - 1] as char); }
            if i + 1 < row.len() { opts.push(row.as_bytes()[i + 1] as char); }
            if r > 0 { if let Some(&b) = ROWS[r - 1].as_bytes().get(i) { opts.push(b as char); } }
            return opts[(x as usize) % opts.len().max(1)];
        }
    }
    c
}

fn r(key: &str, k: usize) -> u64 {
    u64::from_str_radix(&hash(&[key, "noise", &k.to_string()])[..12], 16).unwrap_or(0)
}

/// One typo in a word of >= 4 letters: adjacent-key substitution, dropped, doubled or transposed letter.
fn typo(word: &str, x: u64) -> String {
    let cs: Vec<char> = word.chars().collect();
    if cs.len() < 4 || !cs.iter().all(|c| c.is_ascii_alphabetic()) {
        return word.to_string();
    }
    let i = 1 + (x as usize / 7) % (cs.len() - 2);
    let mut out = cs.clone();
    match x % 4 {
        0 => { let lc = cs[i].to_ascii_lowercase(); out[i] = neighbour(lc, x / 13); }
        1 => { out.remove(i); }
        2 => { out.insert(i, cs[i]); }
        _ => { out.swap(i, i + 1); }
    }
    out.into_iter().collect()
}

/// Noisy form of `q` for persona `p`. Order: known misspellings, random typos, then case and punctuation.
pub fn apply(q: &str, p: &Persona, key: &str) -> String {
    let mut words: Vec<String> = q.split_whitespace().map(str::to_string).collect();
    let mut budget = p.typos as usize;
    if budget > 0 {
        for w in words.iter_mut() {
            let core = w.trim_matches(|c: char| !c.is_alphanumeric() && c != '\'').to_lowercase();
            if let Some((_, m)) = MISSPELL.iter().find(|(a, b)| *a == core && a != b) {
                if r(key, 99) % 2 == 0 {
                    *w = w.to_lowercase().replace(&core, m);
                    budget -= 1;
                    break;
                }
            }
        }
    }
    for k in 0..budget {
        let cands: Vec<usize> = (0..words.len()).filter(|&i| words[i].chars().filter(|c| c.is_ascii_alphabetic()).count() >= 4).collect();
        if cands.is_empty() {
            break;
        }
        let x = r(key, k);
        let i = cands[(x as usize) % cands.len()];
        let (lead, rest): (String, String) = { let w = &words[i]; let n = w.len() - w.trim_start_matches(|c: char| !c.is_alphanumeric()).len(); (w[..n].to_string(), w[n..].to_string()) };
        let end = rest.find(|c: char| !c.is_ascii_alphabetic()).unwrap_or(rest.len());
        words[i] = format!("{lead}{}{}", typo(&rest[..end], x / 3), &rest[end..]);
    }
    let mut s = words.join(" ");
    if p.lowercase {
        s = s.to_lowercase();
    }
    if p.no_punct {
        s = s.chars().filter(|c| !matches!(c, '?' | '.' | ',' | ';' | ':' | '!' | '"')).collect();
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noise_is_deterministic_and_respects_habits() {
        let clean = Persona { typos: 0, ..Default::default() };
        assert_eq!(apply("Does Metformin help?", &clean, "k"), "Does Metformin help?");
        let messy = Persona { typos: 2, lowercase: true, no_punct: true, ..Default::default() };
        let a = apply("Does metformin lower lactate in septic shock?", &messy, "k1");
        assert_eq!(a, apply("Does metformin lower lactate in septic shock?", &messy, "k1"));
        assert!(!a.contains('?') && a == a.to_lowercase());
        assert_ne!(a, "does metformin lower lactate in septic shock");
        assert_eq!(typo("ab", 3), "ab");
    }
}
