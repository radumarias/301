//! Slug validation and generation (random chars, words, names).

use crate::crypto::random_bytes;

/// Base62 without look-alike characters (0 O o 1 l I).
const ALPHABET: &[u8] = b"23456789abcdefghijkmnpqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ";

const ADJECTIVES: &[&str] = &[
    "able", "agile", "amber", "ample", "aqua", "arctic", "autumn", "azure", "bold", "brave",
    "breezy", "bright", "brisk", "bronze", "calm", "candid", "cheery", "chill", "civic", "clever",
    "cobalt", "cosmic", "cozy", "crisp", "curious", "dapper", "daring", "dawn", "deep", "deft",
    "dizzy", "dreamy", "dusky", "eager", "early", "earthy", "easy", "electric", "elegant", "ember",
    "epic", "fair", "fancy", "fast", "fearless", "fierce", "fiery", "fine", "fluffy", "fond",
    "frosty", "gentle", "giant", "gifted", "glad", "golden", "grand", "green", "happy", "hardy",
    "hazy", "hidden", "honest", "humble", "icy", "ideal", "indigo", "jade", "jolly", "jovial",
    "keen", "kind", "lively", "lone", "loud", "lucid", "lucky", "lunar", "magic", "majestic",
    "mellow", "merry", "mighty", "misty", "modern", "noble", "nimble", "nova", "ocean", "olive",
    "orbit", "pale", "patient", "plucky", "polar", "polite", "prime", "proud", "quick", "quiet",
    "radiant", "rapid", "rare", "ready", "regal", "rosy", "royal", "ruby", "rustic", "rusty",
    "sage", "sandy", "sharp", "shiny", "silent", "silver", "simple", "sleek", "smart", "snowy",
    "solar", "solid", "sonic", "spicy", "steady", "stellar", "still", "stormy", "sunny", "super",
    "swift", "tidy", "tiny", "topaz", "tranquil", "true", "twilight", "vast", "velvet", "vivid",
    "warm", "wild", "windy", "wise", "witty", "young", "zany", "zen", "zesty", "zippy",
];

const NOUNS: &[&str] = &[
    "acorn", "anchor", "aurora", "badger", "bay", "beacon", "bear", "beetle", "birch", "bison",
    "breeze", "brook", "canyon", "cedar", "cherry", "cliff", "cloud", "comet", "coral", "cove",
    "crane", "creek", "crow", "dawn", "deer", "delta", "dew", "dolphin", "dove", "dragon", "dune",
    "eagle", "echo", "elk", "ember", "falcon", "fern", "field", "finch", "firefly", "fjord",
    "flame", "forest", "fox", "frog", "frost", "galaxy", "gecko", "glacier", "glade", "grove",
    "gull", "harbor", "hawk", "heron", "hill", "horizon", "island", "ivy", "jaguar", "lagoon",
    "lake", "lark", "leaf", "lemur", "lion", "lotus", "lynx", "maple", "marsh", "meadow", "meteor",
    "mesa", "mist", "moon", "moose", "moss", "mountain", "nebula", "newt", "oak", "oasis", "ocean",
    "orca", "otter", "owl", "panda", "panther", "peak", "pebble", "pine", "planet", "pond",
    "poppy", "prairie", "puffin", "pulsar", "quail", "quasar", "rain", "raven", "reef", "ridge",
    "river", "robin", "rock", "rose", "salmon", "sea", "seal", "shadow", "shore", "sky", "snow",
    "sparrow", "spring", "spruce", "star", "stone", "storm", "stream", "summit", "sun", "swan",
    "thunder", "tide", "tiger", "trail", "tree", "tulip", "valley", "violet", "wave", "willow",
    "wind", "wolf", "wren", "yak", "zebra", "zephyr",
];

const NAMES: &[&str] = &[
    "agnesi",
    "archimedes",
    "babbage",
    "banach",
    "bardeen",
    "bell",
    "bernoulli",
    "bohr",
    "boltzmann",
    "boole",
    "borg",
    "bose",
    "brahe",
    "carson",
    "cerf",
    "chandrasekhar",
    "cohen",
    "copernicus",
    "crick",
    "curie",
    "darwin",
    "davinci",
    "descartes",
    "dijkstra",
    "dirac",
    "edison",
    "einstein",
    "euclid",
    "euler",
    "faraday",
    "fermat",
    "fermi",
    "feynman",
    "fibonacci",
    "franklin",
    "galileo",
    "galois",
    "gauss",
    "germain",
    "godel",
    "goodall",
    "hamilton",
    "hawking",
    "heisenberg",
    "hertz",
    "hilbert",
    "hodgkin",
    "hopper",
    "hubble",
    "huygens",
    "hypatia",
    "jackson",
    "johnson",
    "joule",
    "kepler",
    "khayyam",
    "knuth",
    "kovalevskaya",
    "lamarr",
    "lamport",
    "laplace",
    "lavoisier",
    "leakey",
    "leibniz",
    "lovelace",
    "lumiere",
    "maxwell",
    "mayer",
    "mcclintock",
    "meitner",
    "mendel",
    "mendeleev",
    "minsky",
    "mirzakhani",
    "morse",
    "napier",
    "nash",
    "newton",
    "nightingale",
    "nobel",
    "noether",
    "ohm",
    "pascal",
    "pasteur",
    "pauli",
    "pavlov",
    "perlman",
    "planck",
    "poincare",
    "ramanujan",
    "ritchie",
    "rubin",
    "rutherford",
    "sagan",
    "salk",
    "shannon",
    "shockley",
    "snyder",
    "somerville",
    "stallman",
    "swartz",
    "tesla",
    "thompson",
    "torvalds",
    "turing",
    "volta",
    "wescoff",
    "wilson",
    "wozniak",
    "wright",
    "yalow",
    "yonath",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Style {
    Random,
    Words,
    Names,
}

impl Style {
    pub fn parse(s: &str) -> Style {
        match s {
            "words" => Style::Words,
            "names" => Style::Names,
            _ => Style::Random,
        }
    }
}

fn pick<'a>(list: &[&'a str], r: &[u8]) -> &'a str {
    let n = u16::from_le_bytes([r[0], r[1]]) as usize;
    list[n % list.len()]
}

/// Generates a slug candidate. `attempt` > 0 makes the slug longer to escape collisions.
pub fn generate(style: Style, length: usize, attempt: usize) -> String {
    let r = random_bytes(64);
    match style {
        Style::Random => {
            let len = length.clamp(4, 32) + attempt / 3;
            r.iter()
                .take(len)
                .map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char)
                .collect()
        }
        Style::Words => {
            let n = u16::from_le_bytes([r[4], r[5]]);
            let digits = if attempt < 4 { n % 100 } else { n % 10_000 };
            format!(
                "{}-{}-{}",
                pick(ADJECTIVES, &r[0..2]),
                pick(NOUNS, &r[2..4]),
                digits
            )
        }
        Style::Names => {
            let base = format!("{}-{}", pick(ADJECTIVES, &r[0..2]), pick(NAMES, &r[2..4]));
            if attempt < 3 {
                base
            } else {
                format!("{base}-{}", u16::from_le_bytes([r[4], r[5]]) % 100)
            }
        }
    }
}

const RESERVED: &[&str] = &["_", "api", "robots.txt", "favicon.ico", "favicon.svg"];

/// Trims whitespace and surrounding slashes; `None` when nothing is left.
pub fn normalize(s: &str) -> Option<String> {
    let t = s.trim().trim_matches('/');
    (!t.is_empty()).then(|| t.to_string())
}

/// Validates a custom slug; returns a human-readable reason on failure.
pub fn validate(slug: &str) -> Result<(), String> {
    if slug.is_empty() || slug.len() > 128 {
        return Err("slug must be 1 to 128 characters".into());
    }
    if !slug
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
    {
        return Err("slug may only contain letters, digits, '-', '_', '.' and '/'".into());
    }
    if slug.starts_with('/') || slug.ends_with('/') || slug.contains("//") || slug.contains("..") {
        return Err("slug may not start or end with '/', or contain '//' or '..'".into());
    }
    if RESERVED.contains(&slug) || slug.starts_with("_/") || slug.starts_with("api/") {
        return Err("slug is reserved".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates() {
        assert!(validate("abc").is_ok());
        assert!(validate("blog/2026/launch").is_ok());
        assert!(validate("v1.2_x-y").is_ok());
        assert!(validate("").is_err());
        assert!(validate("api").is_err());
        assert!(validate("api/x").is_err());
        assert!(validate("_/x").is_err());
        assert!(validate("a b").is_err());
        assert!(validate("/a").is_err());
        assert!(validate("a//b").is_err());
        assert!(validate("a/../b").is_err());
        assert!(validate("ünï").is_err());
        assert!(validate(&"a".repeat(129)).is_err());
    }

    #[test]
    fn generated_slugs_are_valid() {
        for style in [Style::Random, Style::Words, Style::Names] {
            for attempt in 0..8 {
                let s = generate(style, 7, attempt);
                assert!(validate(&s).is_ok(), "{s}");
            }
        }
        assert_eq!(generate(Style::Random, 7, 0).len(), 7);
        let w = generate(Style::Words, 7, 0);
        assert_eq!(w.split('-').count(), 3, "{w}");
        let n = generate(Style::Names, 7, 0);
        assert_eq!(n.split('-').count(), 2, "{n}");
    }

    #[test]
    fn word_lists_are_clean() {
        for w in ADJECTIVES.iter().chain(NOUNS).chain(NAMES) {
            assert!(w.chars().all(|c| c.is_ascii_lowercase()), "{w}");
        }
    }
}
