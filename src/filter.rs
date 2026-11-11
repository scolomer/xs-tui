//! Filtre de la liste, ouvert par `f` (SPEC.md § Filtre) : correspondance floue à la fzf. Chaque
//! mot de la requête doit apparaître dans le texte de la session, ses lettres dans l'ordre mais pas
//! forcément côte à côte. Casse intelligente : la casse compte seulement si la requête contient une
//! majuscule. Les lettres accentuées valent leur lettre de base. La pertinence (`score`) trie les
//! sessions ; les lettres trouvées sont surlignées champ par champ (`highlight`).

/// Points par lettre trouvée.
const MATCH: i64 = 16;
/// Pénalité par lettre intercalée entre la première et la dernière lettre trouvées d'un mot.
const GAP: i64 = 3;
/// Bonus d'un mot trouvé à partir d'un début de mot du texte.
const BOUNDARY: i64 = 8;

/// Requête compilée une fois par reconstruction de la liste.
pub struct Query {
    /// Mots de la requête, normalisés par `norm`.
    terms: Vec<Vec<char>>,
    sensitive: bool,
}

impl Query {
    /// `None` pour une requête sans mot : la liste n'est pas filtrée.
    pub fn new(query: &str) -> Option<Query> {
        let sensitive = query.chars().any(char::is_uppercase);
        let terms: Vec<Vec<char>> =
            query.split_whitespace().map(|term| term.chars().map(|c| norm(c, sensitive)).collect()).collect();
        (!terms.is_empty()).then_some(Query { terms, sensitive })
    }

    /// Pertinence de `text`, `None` s'il ne correspond pas : somme, sur les mots de la requête, du
    /// score de leur meilleure place.
    pub fn score(&self, text: &str) -> Option<i64> {
        let chars: Vec<char> = text.chars().map(|c| norm(c, self.sensitive)).collect();
        self.terms.iter().map(|term| best(term, &chars).map(|(score, _, _)| score)).sum()
    }

    /// Lettres de `text` à surligner, une par caractère : pour chaque mot qui correspond à `text`
    /// seul, celles de sa meilleure place. Un mot qui ne correspond qu'à cheval sur deux champs
    /// n'est surligné dans aucun.
    pub fn highlight(&self, text: &str) -> Vec<bool> {
        let chars: Vec<char> = text.chars().map(|c| norm(c, self.sensitive)).collect();
        let mut hits = vec![false; chars.len()];
        for term in &self.terms {
            let Some((_, start, end)) = best(term, &chars) else { continue };
            let mut wanted = term.iter().peekable();
            for (hit, c) in hits[start..=end].iter_mut().zip(&chars[start..=end]) {
                if wanted.peek() == Some(&c) {
                    wanted.next();
                    *hit = true;
                }
            }
        }
        hits
    }
}

/// Meilleure place de `term` dans `text` : (score, première lettre, dernière lettre). Candidats :
/// chaque occurrence d'un bloc, et la fenêtre la plus courte qui finit avec la première
/// correspondance lettre à lettre, comme fzf v1. À score égal, la plus à gauche.
fn best(term: &[char], text: &[char]) -> Option<(i64, usize, usize)> {
    let score = |start: usize, end: usize| {
        let gaps = (end + 1 - start - term.len()) as i64;
        let boundary = start == 0 || !text[start - 1].is_alphanumeric();
        MATCH * term.len() as i64 - GAP * gaps + if boundary { BOUNDARY } else { 0 }
    };
    // Fin de la première correspondance lettre à lettre, puis début au plus près en remontant.
    let mut wanted = term.iter();
    let mut next = wanted.next();
    let end = text.iter().position(|c| {
        if next == Some(c) {
            next = wanted.next();
        }
        next.is_none()
    })?;
    let mut wanted = term.iter().rev();
    let mut next = wanted.next();
    let start = (0..=end).rev().find(|&i| {
        if next == Some(&text[i]) {
            next = wanted.next();
        }
        next.is_none()
    })?;
    let mut best = (score(start, end), start, end);
    for (s, _) in text.windows(term.len()).enumerate().filter(|(_, w)| *w == term) {
        let candidate = score(s, s + term.len() - 1);
        if candidate > best.0 || (candidate == best.0 && s < best.1) {
            best = (candidate, s, s + term.len() - 1);
        }
    }
    Some(best)
}

/// Caractère comparé par le filtre : lettre de base, en minuscule sauf si la casse compte. Un
/// caractère pour un caractère, pour que `highlight` désigne les bonnes positions.
fn norm(c: char, sensitive: bool) -> char {
    let c = fold(c);
    if sensitive { c } else { c.to_lowercase().next().unwrap_or(c) }
}

/// Lettre de base d'une lettre latine accentuée, casse conservée ; autre caractère inchangé.
fn fold(c: char) -> char {
    let lower = if c.is_uppercase() { c.to_lowercase().next().unwrap_or(c) } else { c };
    let base = match lower {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' => 'a',
        'ç' => 'c',
        'è' | 'é' | 'ê' | 'ë' => 'e',
        'ì' | 'í' | 'î' | 'ï' => 'i',
        'ñ' => 'n',
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' => 'o',
        'ù' | 'ú' | 'û' | 'ü' => 'u',
        'ý' | 'ÿ' => 'y',
        _ => return c,
    };
    if c.is_uppercase() { base.to_ascii_uppercase() } else { base }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches(query: &str, text: &str) -> bool {
        Query::new(query).unwrap().score(text).is_some()
    }

    #[test]
    fn every_word_must_match_as_an_ordered_subsequence() {
        assert!(matches("xst main", "xs-tui main Add filter"));
        assert!(matches("adfl", "Add filter"));
        assert!(!matches("fa", "af"));
        assert!(!matches("xst dev", "xs-tui main Add filter"));
    }

    #[test]
    fn case_matters_only_with_an_uppercase_letter_in_the_query() {
        assert!(matches("push", "PUSH: List slow selections"));
        assert!(matches("PUSH", "PUSH: List slow selections"));
        assert!(!matches("Push", "PUSH: List slow selections"));
    }

    #[test]
    fn accented_letters_match_their_base_letter() {
        assert!(matches("deploiement", "Déploiement à l'échelle"));
        assert!(matches("déploiement", "Deploiement"));
        assert!(matches("Ecran", "Écran"));
    }

    #[test]
    fn a_blank_query_filters_nothing() {
        assert!(Query::new("  ").is_none());
    }

    #[test]
    fn a_block_at_a_word_start_scores_above_scattered_letters() {
        let score = |query: &str, text: &str| Query::new(query).unwrap().score(text).unwrap();
        assert!(score("code", "Revue de code") > score("code", "Corriger le mode"));
        assert!(score("dep", "Déploiement") > score("dep", "Index deep"));
    }

    #[test]
    fn highlight_prefers_a_contiguous_block_and_skips_words_missing_from_the_field() {
        let marks = |query: &str, text: &str| -> String {
            let hits = Query::new(query).unwrap().highlight(text);
            text.chars().zip(hits).map(|(c, hit)| if hit { c } else { '.' }).collect()
        };
        // `de` existe d'un bloc dans « Déploiement » : pas les premières lettres éparses.
        assert_eq!(marks("de", "Add Déploiement"), "....Dé.........");
        assert_eq!(marks("adfl", "Add filter"), "Ad..f.l...");
        // `zz` ne correspond pas à ce champ : seul `add` y est surligné.
        assert_eq!(marks("add zz", "Add filter"), "Add.......");
    }
}
