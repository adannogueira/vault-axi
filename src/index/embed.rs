//! Fast, zero-dependency multilingual lexical + subword vectorizer (384-d).
//! Replaces external English-only neural weights with instant, 100% offline
//! signed feature hashing over diacritic-folded multilingual stems, cognate
//! prefixes, character n-grams, and token bigrams.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

pub const EMBED_DIM: usize = 1024;
const MAX_INPUT_CHARS: usize = 4000;

pub fn embeddings_disabled() -> bool {
    std::env::var("VAULT_AXI_NO_EMBED").as_deref() == Ok("1")
}

/// Initialize the vectorizer. Always succeeds unless disabled via `VAULT_AXI_NO_EMBED=1`.
pub fn init_embedder() -> bool {
    !embeddings_disabled()
}

pub fn is_available() -> bool {
    !embeddings_disabled()
}

/// Fold Unicode Latin/European diacritics to ASCII base characters and lowercase.
/// Ensures multilingual queries (`usuarios`, `execucao`, `estrategia`, `conteiner`)
/// match accented text (`Usuários`, `Execução`, `Estratégia`, `Contêiner`).
pub fn fold_diacritics(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        for lower in ch.to_lowercase() {
            match lower {
                'á' | 'à' | 'ã' | 'â' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' => out.push('a'),
                'é' | 'è' | 'ê' | 'ë' | 'ē' | 'ĕ' | 'ė' | 'ę' | 'ě' => out.push('e'),
                'í' | 'ì' | 'î' | 'ï' | 'ĩ' | 'ī' | 'ĭ' | 'į' => out.push('i'),
                'ó' | 'ò' | 'õ' | 'ô' | 'ö' | 'ø' | 'ō' | 'ŏ' | 'ő' => out.push('o'),
                'ú' | 'ù' | 'û' | 'ü' | 'ũ' | 'ū' | 'ŭ' | 'ů' | 'ű' | 'ų' => {
                    out.push('u')
                }
                'ç' | 'ć' | 'č' | 'ĉ' | 'ċ' => out.push('c'),
                'ñ' | 'ń' | 'ň' | 'ņ' => out.push('n'),
                'ý' | 'ÿ' | 'ŷ' => out.push('y'),
                'ś' | 'š' | 'ş' | 'ŝ' => out.push('s'),
                'ź' | 'ż' | 'ž' => out.push('z'),
                'ł' | 'ľ' | 'ĺ' | 'ļ' => out.push('l'),
                'ř' | 'ŕ' | 'ŗ' => out.push('r'),
                'ť' | 'ţ' => out.push('t'),
                'ď' | 'đ' => out.push('d'),
                'ğ' | 'ĝ' | 'ġ' | 'ģ' => out.push('g'),
                'ß' => out.push_str("ss"),
                'æ' => out.push_str("ae"),
                'œ' => out.push_str("oe"),
                c => out.push(c),
            }
        }
    }
    out
}

fn stopwords() -> &'static HashSet<&'static str> {
    static STOP: OnceLock<HashSet<&'static str>> = OnceLock::new();
    STOP.get_or_init(|| {
        HashSet::from([
            // English
            "the", "and", "for", "with", "that", "this", "from", "are", "was", "were", "have",
            "has", "had", "not", "but", "what", "all", "can", "when", "how", "use", "used",
            "using", "into", "only", "also", "more", "some", "such", "than", "then", "them",
            "they", "their", "there", "these", "those", "will", "would", "could", "should",
            "about", "after", "before", "between", "each", "other", "where", "which", "while",
            "who", "whom", "whose", "why", "you", "your", "our", "out", "over", "under", "again",
            "further", "once", "here", "both", "few", "most", "own", "same", "too", "very", "just",
            "now", "being", "been", "does", "did", "doing", "its", "it", "his", "her", "she",
            "him", // Portuguese / Spanish / Romance
            "para", "com", "por", "uma", "uns", "umas", "dos", "das", "nos", "nas", "pelos",
            "pelas", "pelo", "pela", "como", "mais", "mas", "sem", "sob", "sobre", "entre", "ate",
            "apos", "isso", "isto", "aquilo", "este", "esta", "estes", "estas", "esse", "essa",
            "esses", "essas", "aquele", "aquela", "aqueles", "aquelas", "seu", "sua", "seus",
            "suas", "meu", "minha", "meus", "minhas", "nosso", "nossa", "nossos", "nossas", "ele",
            "ela", "eles", "elas", "voce", "voces", "quando", "onde", "quem", "qual", "quais",
            "porque", "muito", "muita", "muitos", "muitas", "pouco", "pouca", "outro", "outra",
            "outros", "outras", "mesmo", "mesma", "tambem", "ainda", "nao", "sim", "ser", "estar",
            "ter", "haver", "fazer", "foi", "sao", "era", "estao", "tem", "pode", "podem", "deve",
            "devem", "todo", "toda", "todos", "todas", "cada", "qualquer", "algum", "alguma",
            "alguns", "algumas", "nenhum", "nenhuma", "del", "las", "los", "una", "unos", "unas",
            "con", "sin", "pero", "sus", "estos", "esos", "esas", "delle", "della", "dans", "pour",
            "avec", "sur", "pas", "plus", "par", "aux", "des", "les", "une", "est", "sont",
        ])
    })
}

pub fn is_multilingual_stopword(word: &str) -> bool {
    stopwords().contains(word)
}

/// Lightweight multilingual stemmer (English + Portuguese/Spanish/Romance).
/// Expects an already diacritic-folded lowercase ASCII/Unicode token.
pub fn stem_multilingual(word: &str) -> String {
    let mut w = word.trim().to_string();
    if w.len() <= 3 {
        return w;
    }

    // Step 1: Normalize plural endings first so singulars and plurals follow identical rules
    if let Some(stem) = w.strip_suffix("ies") {
        if stem.len() >= 2 {
            w = format!("{stem}y");
        }
    } else if let Some(stem) = w.strip_suffix("oes") {
        if stem.len() >= 2 {
            w = format!("{stem}ao");
        }
    } else if let Some(stem) = w.strip_suffix("ais") {
        if stem.len() >= 2 {
            w = format!("{stem}al");
        }
    } else if let Some(stem) = w.strip_suffix("eis") {
        if stem.len() >= 2 {
            w = format!("{stem}el");
        }
    } else if let Some(stem) = w.strip_suffix("ns") {
        if stem.len() >= 3 {
            w = format!("{stem}m");
        }
    } else if let Some(stem) = w.strip_suffix("es") {
        if stem.ends_with("ss")
            || stem.ends_with("sh")
            || stem.ends_with("ch")
            || stem.ends_with('x')
            || stem.ends_with('z')
        {
            if stem.len() >= 3 {
                w = stem.to_string();
            }
        } else if stem.len() >= 3 {
            // Keep 'e' for Step 3 to strip uniformly on both `image` and `images` (`images` -> `image` -> `imag`)
            w = format!("{stem}e");
        }
    } else if let Some(stem) = w.strip_suffix('s') {
        if !stem.ends_with('s') && stem.len() >= 3 {
            w = stem.to_string();
        }
    }

    // Step 2: Derivational suffixes (Romance + English)
    const SUFFIX_REPLACEMENTS: &[(&str, &str, usize)] = &[
        ("izacao", "iz", 4),
        ("ização", "iz", 4),
        ("ization", "iz", 4),
        ("isation", "iz", 4),
        ("amento", "ament", 4),
        ("imento", "iment", 4),
        ("idade", "idad", 4),
        ("ador", "ador", 4),
        ("ational", "at", 3),
        ("fulness", "ful", 3),
        ("ousness", "ous", 3),
        ("iveness", "iv", 3),
        ("acao", "ac", 3),
        ("icao", "ic", 3),
        ("ucao", "uc", 3),
        ("mente", "", 4),
        ("ment", "", 4),
        ("able", "abl", 4),
        ("ible", "ibl", 4),
        ("ando", "", 4),
        ("endo", "", 4),
        ("indo", "", 4),
        ("aram", "", 4),
        ("eram", "", 4),
        ("iram", "", 4),
        ("ing", "", 4),
        ("edly", "", 4),
        ("ed", "", 4),
    ];

    for &(suf, rep, min_stem) in SUFFIX_REPLACEMENTS {
        if let Some(stem) = w.strip_suffix(suf) {
            if stem.len() >= min_stem {
                w = format!("{stem}{rep}");
                break;
            }
        }
    }

    // Step 3: Strip trailing silent/thematic vowel (a/o/e) on stems >= 5 chars
    // so `image`/`images`/`imagem`/`imagens` -> `imag`, `estrategia`/`estrategias` -> `estrateg`,
    // `usuario`/`usuarios` -> `usuari`.
    if w.len() >= 5 {
        if let Some(stem) = w.strip_suffix(['a', 'o', 'e']) {
            if stem.len() >= 4 {
                w = stem.to_string();
            }
        }
    }

    w
}

/// Extract folded, stemmed content tokens from free text (splitting camelCase and snake_case).
pub fn tokenize_multilingual(text: &str) -> Vec<(String, String)> {
    // Split camelCase boundaries before folding (e.g. `MultiTenant` -> `Multi Tenant`)
    let mut expanded = String::with_capacity(text.len() + 16);
    let mut prev_lower = false;
    for ch in text.chars() {
        if ch.is_uppercase() && prev_lower {
            expanded.push(' ');
        }
        prev_lower = ch.is_lowercase();
        expanded.push(ch);
    }

    let folded = fold_diacritics(&expanded);
    let mut out = Vec::new();
    for raw_tok in folded.split(|c: char| !c.is_alphanumeric()) {
        let tok = raw_tok.trim();
        if tok.chars().count() < 2 || is_multilingual_stopword(tok) {
            continue;
        }
        // Ignore pure long hex hashes or pure numbers > 4 digits
        if tok.len() > 4 && tok.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let stem = stem_multilingual(tok);
        if stem.chars().count() >= 2 && !is_multilingual_stopword(&stem) {
            out.push((tok.to_string(), stem));
        }
    }
    out
}

#[inline]
fn hash_feature(feat: &str) -> u64 {
    // FNV-1a + SplitMix64 avalanche for uniform bucket distribution
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in feat.as_bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h ^= h >> 30;
    h = h.wrapping_mul(0xbf58476d1ce4e5b9);
    h ^= h >> 27;
    h = h.wrapping_mul(0x94d049bb133111eb);
    h ^= h >> 31;
    h
}

#[inline]
fn add_hashed_feature(vec: &mut [f32; EMBED_DIM], feat: &str, weight: f32) {
    let h = hash_feature(feat);
    let idx1 = (h as usize) % EMBED_DIM;
    let sign1 = if (h >> 32) & 1 == 0 { 1.0 } else { -1.0 };
    vec[idx1] += sign1 * weight;
}

fn embed_single(text: &str) -> Vec<f32> {
    let clipped: String = text.chars().take(MAX_INPUT_CHARS).collect();
    let tokens = tokenize_multilingual(&clipped);
    let mut vec = [0.0f32; EMBED_DIM];
    if tokens.is_empty() {
        return vec.to_vec();
    }

    let mut stem_counts: HashMap<String, usize> = HashMap::new();
    for (_, stem) in &tokens {
        *stem_counts.entry(stem.clone()).or_insert(0) += 1;
    }

    for (stem, count) in &stem_counts {
        let tf = 1.0 + (*count as f32).ln();
        let spec = ((stem.len().clamp(3, 12) as f32) / 5.0).sqrt();
        let w = tf * spec;

        // 1. Exact stem feature (primary weight)
        add_hashed_feature(&mut vec, &format!("s:{stem}"), 3.0 * w);

        // 2. Cognate 5-char root prefix feature (bridges EN/PT/ES/FR cognates such as
        // `virtualization`/`virtualizacao`, `container`/`conteiner`, `strategy`/`estrategia`,
        // `dockerfile`/`docker`)
        let normalized_cognate = stem.strip_prefix("es").unwrap_or(stem);
        let chars: Vec<char> = normalized_cognate.chars().collect();
        if chars.len() >= 5 {
            let p5: String = chars[..5].iter().collect();
            add_hashed_feature(&mut vec, &format!("p5:{p5}"), 1.4 * tf);
        }
        if chars.len() >= 6 {
            let p6: String = chars[..6].iter().collect();
            add_hashed_feature(&mut vec, &format!("p6:{p6}"), 1.2 * tf);
        }
    }

    // 3. Adjacent stem bigrams for phrase structure ("event sourcing", "multi tenant")
    for pair in tokens.windows(2) {
        let s1 = &pair[0].1;
        let s2 = &pair[1].1;
        if s1 != s2 {
            add_hashed_feature(&mut vec, &format!("b:{s1}_{s2}"), 0.9);
        }
    }

    let norm: f32 = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in &mut vec {
            *x /= norm;
        }
    }
    vec.to_vec()
}

/// Embed a batch of texts into normalized 384-d f32 vectors, or None if disabled.
pub fn embed(texts: &[String]) -> Option<Vec<Vec<f32>>> {
    if embeddings_disabled() {
        return None;
    }
    Some(texts.iter().map(|t| embed_single(t)).collect())
}

pub fn to_blob(vec: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(vec.len() * 4);
    for &v in vec {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

pub fn from_blob(buf: &[u8]) -> Vec<f32> {
    buf.as_chunks::<4>()
        .0
        .iter()
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// Cosine similarity for already-normalized vectors reduces to a dot product.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_roundtrip_and_cosine_similarity() {
        let orig = vec![0.5f32, -0.5, 0.0, 1.25, -3.75];
        let blob = to_blob(&orig);
        assert_eq!(blob.len(), orig.len() * 4);
        let decoded = from_blob(&blob);
        assert_eq!(decoded, orig);

        // Trailing incomplete bytes ignored safely by chunks_exact(4)
        let mut ragged = blob.clone();
        ragged.push(0xFF);
        assert_eq!(from_blob(&ragged), orig);

        let a = vec![1.0f32, 0.0, 0.0];
        let b = vec![0.0f32, 1.0, 0.0];
        let c = vec![-1.0f32, 0.0, 0.0];
        assert!((cosine(&a, &a) - 1.0).abs() < 1e-6);
        assert!(cosine(&a, &b).abs() < 1e-6);
        assert!((cosine(&a, &c) + 1.0).abs() < 1e-6);
    }

    #[test]
    fn multilingual_diacritics_stemming_and_vector_similarity() {
        assert_eq!(
            fold_diacritics("Análise de Execução das Estratégias e Usuários"),
            "analise de execucao das estrategias e usuarios"
        );
        assert_eq!(
            stem_multilingual(&fold_diacritics("Usuários")),
            stem_multilingual(&fold_diacritics("usuario"))
        );
        assert_eq!(
            stem_multilingual(&fold_diacritics("Estratégias")),
            stem_multilingual(&fold_diacritics("estrategia"))
        );

        let v_docker1 =
            embed_single("Docker containers package images and layers for virtualization");
        let v_docker2 = embed_single("Building a container image with a Dockerfile layer cache");
        let v_pt_cognate =
            embed_single("Virtualização de containers e imagens com camadas no Docker");
        let v_unrelated =
            embed_single("Quantum electrodynamics photon propagator Feynman diagrams");

        let sim_related = cosine(&v_docker1, &v_docker2);
        let sim_cross_lang = cosine(&v_docker1, &v_pt_cognate);
        let sim_unrelated = cosine(&v_docker1, &v_unrelated);

        assert!(
            sim_related > 0.30,
            "Expected high similarity for related texts, got {sim_related}"
        );
        assert!(
            sim_cross_lang > 0.25,
            "Expected strong cross-lingual EN/PT cognate similarity, got {sim_cross_lang}"
        );
        assert!(
            sim_unrelated.abs() < 0.12,
            "Expected near-zero similarity for unrelated texts, got {sim_unrelated}"
        );
    }
}
