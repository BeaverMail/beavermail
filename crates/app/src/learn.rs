// SPDX-License-Identifier: AGPL-3.0-only

//! A spam classifier learnt from the user's own mail: a logistic regression trained online with
//! FTRL-Proximal over hashed word features. It is trained again from the archive each time new mail is
//! checked: the messages the user called spam or not spam (kept as examples, so they outlive the
//! server emptying its Spam folder), the Spam folder itself, and the mail of known senders. Only the
//! examples are kept, never the weights, so what it knows is always what the archive says.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

/// Fewer examples of either kind than this and the model says nothing: rules decide alone until then.
pub const MIN_EXAMPLES: usize = 200;
const EPOCHS: usize = 5;
// FTRL-Proximal settings: learning rate, its smoothing, L1 and L2.
const ALPHA: f32 = 0.3;
const BETA: f32 = 1.0;
const L1: f32 = 1.0;
const L2: f32 = 1.0;

/// The trained weights, sparse: only features seen in training have a slot.
pub struct Model {
    /// Per feature: FTRL's accumulated gradient `z` and squared-gradient sum `n`.
    slots: HashMap<u64, (f32, f32)>,
    /// A readable name for each feature, to say which words weighed.
    names: HashMap<u64, String>,
    /// Words left out of every text: see [`COMMON`].
    common: HashSet<String>,
}

/// A word in at least this share of the spam and of the other mail is one of a language's own ("de",
/// "the", "di"), in any language: it would teach the model which language spam is in, not what it says.
const COMMON: f32 = 0.1;

/// The words of a text, lower case, the first 300.
fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric()).filter(|w| w.chars().count() >= 2).map(str::to_lowercase).take(300).collect()
}

/// The features of a text's `words` but the `common` ones: each word, each pair of words up to four
/// apart (orthogonal sparse bigrams: "pink gelatin" and "pink _ ritual" both count), and the sender's domain.
fn features(words: &[String], domain: &str, common: &HashSet<String>) -> Vec<(u64, String)> {
    let words: Vec<&String> = words.iter().filter(|w| !common.contains(*w)).collect();
    let mut out: Vec<String> = words.iter().map(|w| (*w).clone()).collect();
    for (i, first) in words.iter().enumerate() {
        for (gap, second) in words.iter().skip(i + 1).take(4).enumerate() {
            out.push(format!("{first} {}{second}", "_ ".repeat(gap)));
        }
    }
    if !domain.is_empty() {
        out.push(format!("@{domain}"));
    }
    out.sort();
    out.dedup();
    out.into_iter()
        .map(|f| {
            let mut hasher = std::hash::DefaultHasher::new();
            f.hash(&mut hasher);
            (hasher.finish(), f)
        })
        .collect()
}

fn tokenized(examples: &[(String, String)]) -> Vec<(Vec<String>, &str)> {
    examples.iter().map(|(t, d)| (words(t), d.as_str())).collect()
}

/// The words in at least [`COMMON`] of the texts of each kind.
fn common_words(spam: &[(Vec<String>, &str)], ham: &[(Vec<String>, &str)]) -> HashSet<String> {
    let share = |texts: &[(Vec<String>, &str)]| {
        let mut count: HashMap<&str, usize> = HashMap::new();
        for (words, _) in texts {
            for word in words.iter().map(String::as_str).collect::<HashSet<_>>() {
                *count.entry(word).or_default() += 1;
            }
        }
        count.into_iter().filter(|(_, n)| *n as f32 >= texts.len() as f32 * COMMON).map(|(w, _)| w.to_owned()).collect::<HashSet<String>>()
    };
    share(spam).intersection(&share(ham)).cloned().collect()
}

impl Model {
    /// Trains on (text, sender domain) examples; `None` until both kinds have [`MIN_EXAMPLES`].
    pub fn train(spam: &[(String, String)], ham: &[(String, String)]) -> Option<Self> {
        if spam.len() < MIN_EXAMPLES || ham.len() < MIN_EXAMPLES {
            return None;
        }
        let (spam, ham) = (tokenized(spam), tokenized(ham));
        let mut model = Self { slots: HashMap::new(), names: HashMap::new(), common: common_words(&spam, &ham) };
        // Interleaved, so neither kind comes in one block; the larger kind is not repeated, the
        // probability threshold accounts for it.
        let examples: Vec<(Vec<(u64, String)>, f32)> = {
            let (mut s, mut h) = (spam.iter().map(|(w, d)| (features(w, d, &model.common), 1.0)), ham.iter().map(|(w, d)| (features(w, d, &model.common), 0.0)));
            let mut out = Vec::new();
            loop {
                match (s.next(), h.next()) {
                    (None, None) => break,
                    (a, b) => out.extend(a.into_iter().chain(b)),
                }
            }
            out
        };
        for _ in 0..EPOCHS {
            for (features, label) in &examples {
                model.learn(features, *label);
            }
        }
        for (features, _) in examples {
            model.names.extend(features);
        }
        Some(model)
    }

    fn weight(&self, feature: u64) -> f32 {
        let Some(&(z, n)) = self.slots.get(&feature) else { return 0.0 };
        if z.abs() <= L1 { 0.0 } else { -(z - z.signum() * L1) / ((BETA + n.sqrt()) / ALPHA + L2) }
    }

    fn predict(&self, features: &[(u64, String)]) -> f32 {
        let sum: f32 = features.iter().map(|(f, _)| self.weight(*f)).sum();
        1.0 / (1.0 + (-sum.clamp(-35.0, 35.0)).exp())
    }

    fn learn(&mut self, features: &[(u64, String)], label: f32) {
        let gradient = self.predict(features) - label;
        for (f, _) in features {
            let weight = self.weight(*f);
            let (z, n) = self.slots.entry(*f).or_insert((0.0, 0.0));
            let sigma = ((*n + gradient * gradient).sqrt() - n.sqrt()) / ALPHA;
            *z += gradient - sigma * weight;
            *n += gradient * gradient;
        }
    }

    /// The probability that a text from `domain` is spam, and the features that pushed it up most.
    pub fn score(&self, text: &str, domain: &str) -> (f32, Vec<String>) {
        let features = features(&words(text), domain, &self.common);
        let mut pushing: Vec<(f32, &str)> = features.iter().map(|(f, name)| (self.weight(*f), name.as_str())).filter(|(w, _)| *w > 0.0).collect();
        pushing.sort_by(|a, b| b.0.total_cmp(&a.0));
        (self.predict(&features), pushing.into_iter().take(4).map(|(_, n)| n.to_owned()).collect())
    }

    /// How many features carry a weight: zero means the model learnt nothing it would act on.
    pub fn size(&self) -> usize {
        self.slots.keys().filter(|f| self.weight(**f) != 0.0).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn examples(texts: &[&str], domain: &str) -> Vec<(String, String)> {
        texts.iter().cycle().take(MIN_EXAMPLES + 5).enumerate().map(|(i, t)| (format!("{t} n{i}"), domain.to_owned())).collect()
    }

    #[test]
    fn it_learns_the_words_of_spam_and_of_the_users_mail() {
        let spam = examples(&["korean pink gelatin ritual for mature skin", "slim heater with remote and timer limited offer", "you have won a gift card claim now"], "spam.example");
        let ham = examples(&["invoice for your server renewal attached", "re: meeting tomorrow about the project", "your domain transfer is complete"], "work.example");
        let model = Model::train(&spam, &ham).unwrap();
        let (p, why) = model.score("the pink gelatin ritual that lifts skin", "other.example");
        assert!(p > 0.75, "{p}");
        assert!(why.iter().any(|w| w.contains("gelatin") || w.contains("ritual") || w.contains("skin")), "{why:?}");
        assert!(model.score("invoice for the domain renewal", "other.example").0 < 0.3);
        assert!(model.size() > 0);
    }

    #[test]
    fn the_words_of_a_language_are_not_features() {
        let spam: Vec<(String, String)> = (0..MIN_EXAMPLES).map(|i| (format!("oferta{i} de la crema gelatina"), "spam.example".into())).collect();
        let ham: Vec<(String, String)> = (0..MIN_EXAMPLES).map(|i| (format!("factura{i} de la casa"), "work.example".into())).collect();
        let model = Model::train(&spam, &ham).unwrap();
        assert!(model.common.contains("de") && model.common.contains("la"));
        assert!(!model.common.contains("gelatina"), "a word only spam has is what it says");
        let (_, why) = model.score("tu pedido de la tienda gelatina", "other.example");
        assert!(why.iter().all(|w| !w.split(' ').any(|p| p == "de" || p == "la")), "{why:?}");
    }

    #[test]
    fn too_few_examples_and_it_stays_silent() {
        let few: Vec<(String, String)> = vec![("a b c".into(), "x".into()); MIN_EXAMPLES - 1];
        let many: Vec<(String, String)> = vec![("d e f".into(), "y".into()); MIN_EXAMPLES];
        assert!(Model::train(&few, &many).is_none());
    }
}
