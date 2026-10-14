// SPDX-License-Identifier: AGPL-3.0-only

//! A spam classifier learnt from the user's own mail: a logistic regression trained online with
//! FTRL-Proximal over hashed word features. It is trained again from the archive each time new mail is
//! checked: the messages the user called spam or not spam (kept as examples, so they outlive the
//! server emptying its Spam folder), the Spam folder itself, and the mail of known senders. Only the
//! examples are kept, never the weights, so what it knows is always what the archive says.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};

/// Fewer examples of either kind than this and the model says nothing: rules decide alone until then.
pub const MIN_EXAMPLES: usize = 20;
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
}

/// The features of a text: each word, each pair of words up to four apart (orthogonal sparse
/// bigrams: "pink gelatin" and "pink _ ritual" both count), and the sender's domain.
fn features(text: &str, domain: &str) -> Vec<(u64, String)> {
    let words: Vec<String> = text.split(|c: char| !c.is_alphanumeric()).filter(|w| w.chars().count() >= 2).map(str::to_lowercase).take(300).collect();
    let mut out: Vec<String> = words.clone();
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

impl Model {
    /// Trains on (text, sender domain) examples; `None` until both kinds have [`MIN_EXAMPLES`].
    pub fn train(spam: &[(String, String)], ham: &[(String, String)]) -> Option<Self> {
        if spam.len() < MIN_EXAMPLES || ham.len() < MIN_EXAMPLES {
            return None;
        }
        let mut model = Self { slots: HashMap::new(), names: HashMap::new() };
        // Interleaved, so neither kind comes in one block; the larger kind is not repeated, the
        // probability threshold accounts for it.
        let examples: Vec<(Vec<(u64, String)>, f32)> = {
            let (mut s, mut h) = (spam.iter().map(|(t, d)| (features(t, d), 1.0)), ham.iter().map(|(t, d)| (features(t, d), 0.0)));
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
        let features = features(text, domain);
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
        assert!(p > 0.8, "{p}");
        assert!(why.iter().any(|w| w.contains("gelatin") || w.contains("ritual") || w.contains("skin")), "{why:?}");
        assert!(model.score("invoice for the domain renewal", "other.example").0 < 0.3);
        assert!(model.size() > 0);
    }

    #[test]
    fn too_few_examples_and_it_stays_silent() {
        let few: Vec<(String, String)> = vec![("a b c".into(), "x".into()); MIN_EXAMPLES - 1];
        let many: Vec<(String, String)> = vec![("d e f".into(), "y".into()); MIN_EXAMPLES];
        assert!(Model::train(&few, &many).is_none());
    }
}
