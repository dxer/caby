//! Lightweight in-memory intent matcher.
//!
//! Scores a task query against the corpus of skill documents (name +
//! description + keywords). Uses TF-IDF weighted cosine similarity with an
//! exact-name bonus. CJK text is handled by emitting per-character and
//! character-bigram tokens so Chinese skill names match Chinese queries.

use std::collections::{HashMap, HashSet};

use crate::util::is_cjk;

const STOPWORDS: &[&str] = &[
    "a", "an", "the", "and", "or", "but", "of", "to", "for", "on", "in", "with", "at", "by",
    "is", "are", "was", "were", "be", "been", "do", "does", "did", "it", "this", "that", "i",
    "we", "you", "they", "my", "your", "our", "as", "if", "then", "than", "so", "into", "from",
    "请", "的", "了", "和", "与", "在", "是", "要", "我", "你", "他", "它", "一个", "进行", "使用",
];

/// Tokenize text: lowercase ASCII words, CJK unigrams+bigrams.
pub fn tokenize(text: &str) -> Vec<String> {
    let mut tokens: Vec<String> = Vec::new();
    let mut word = String::new();
    let mut cjk_run: Vec<char> = Vec::new();

    for ch in text.chars() {
        if is_cjk(ch) {
            flush_word(&mut tokens, &mut word);
            cjk_run.push(ch);
        } else if ch.is_alphanumeric()
            || ch == '_'
            || ch == '-'
            || ch == '/'
            || ch == ':'
            || ch == '.'
        {
            if !cjk_run.is_empty() {
                flush_cjk(&mut tokens, &mut cjk_run);
            }
            word.push(ch);
        } else {
            flush_word(&mut tokens, &mut word);
            if !cjk_run.is_empty() {
                flush_cjk(&mut tokens, &mut cjk_run);
            }
        }
    }
    flush_word(&mut tokens, &mut word);
    flush_cjk(&mut tokens, &mut cjk_run);
    tokens
}

fn flush_word(tokens: &mut Vec<String>, word: &mut String) {
    if !word.is_empty() {
        let w = word.to_ascii_lowercase();
        if !STOPWORDS.contains(&w.as_str()) {
            tokens.push(w);
        }
        word.clear();
    }
}

/// Emit CJK bigrams only — single CJK chars are noise for intent matching
/// ("查" alone shouldn't make a DB skill match a code-review query).
fn flush_cjk(tokens: &mut Vec<String>, run: &mut Vec<char>) {
    if run.len() < 2 {
        run.clear();
        return;
    }
    for i in 0..run.len() - 1 {
        let mut bg = String::with_capacity(4);
        bg.push(run[i]);
        bg.push(run[i + 1]);
        tokens.push(bg);
    }
    run.clear();
}

/// A document = token -> term frequency.
#[derive(Debug, Clone)]
pub struct Doc {
    pub id: String,
    pub tokens: Vec<String>,
    tf: HashMap<String, f64>,
    norm: f64,
}

impl Doc {
    pub fn build(id: impl Into<String>, text: &str) -> Doc {
        let tokens = tokenize(text);
        let mut tf: HashMap<String, f64> = HashMap::new();
        for t in &tokens {
            *tf.entry(t.clone()).or_insert(0.0) += 1.0;
        }
        let norm = (tf.values().map(|v| v * v).sum::<f64>()).sqrt().max(1e-9);
        Doc {
            id: id.into(),
            tokens,
            tf,
            norm,
        }
    }
}

#[derive(Debug)]
pub struct Matcher {
    docs: Vec<Doc>,
    /// token -> document frequency
    df: HashMap<String, usize>,
    idf: HashMap<String, f64>,
    doc_count: f64,
}

impl Default for Matcher {
    fn default() -> Self {
        Self::new()
    }
}

impl Matcher {
    pub fn new() -> Matcher {
        Matcher {
            docs: Vec::new(),
            df: HashMap::new(),
            idf: HashMap::new(),
            doc_count: 0.0,
        }
    }

    pub fn rebuild(&mut self, docs: Vec<Doc>) {
        self.docs = docs;
        let mut df: HashMap<String, usize> = HashMap::new();
        for doc in &self.docs {
            let mut seen: HashSet<String> = HashSet::new();
            for t in &doc.tokens {
                if seen.insert(t.clone()) {
                    *df.entry(t.clone()).or_insert(0) += 1;
                }
            }
        }
        self.doc_count = self.docs.len().max(1) as f64;
        self.idf = df
            .iter()
            .map(|(t, d)| (t.clone(), (1.0 + self.doc_count / (*d as f64)).ln() + 1.0))
            .collect();
        self.df = df;
    }

    #[allow(dead_code)]
    pub fn corpus_size(&self) -> usize {
        self.docs.len()
    }

    /// Compute the relevance of every document against `query`.
    /// Returns (doc_id, score) sorted descending by score.
    pub fn rank(&self, query: &str, top_k: usize) -> Vec<(String, f64)> {
        if self.docs.is_empty() {
            return Vec::new();
        }
        if query.trim().is_empty() {
            // no query: return everything, name-sorted
            let mut all: Vec<(String, f64)> = self
                .docs
                .iter()
                .map(|d| (d.id.clone(), 0.0))
                .collect();
            all.sort_by(|a, b| a.0.cmp(&b.0));
            if top_k == 0 {
                return all;
            }
            all.truncate(top_k);
            return all;
        }

        let q_tokens = tokenize(query);
        if q_tokens.is_empty() {
            // query had no usable tokens (e.g. only stopwords or lone CJK chars):
            // return everything like a blank query
            let mut all: Vec<(String, f64)> = self.docs.iter().map(|d| (d.id.clone(), 0.0)).collect();
            all.sort_by(|a, b| a.0.cmp(&b.0));
            if top_k > 0 {
                all.truncate(top_k);
            }
            return all;
        }
        // query tf-idf vector
        let mut qt: HashMap<String, f64> = HashMap::new();
        for t in &q_tokens {
            *qt.entry(t.clone()).or_insert(0.0) += 1.0;
        }
        let query_norm: f64 = qt
            .iter()
            .map(|(t, v)| {
                let idf = self.idf.get(t).copied().unwrap_or(1.0);
                let w = v * idf;
                w * w
            })
            .sum::<f64>()
            .sqrt()
            .max(1e-9);

        // exact-name bonus: if the query text contains the doc name (or vice versa)
        let q_lower = query.to_lowercase();

        let mut results: Vec<(String, f64)> = Vec::with_capacity(self.docs.len());
        for doc in &self.docs {
            let mut score = 0.0;
            for (term, &tf) in &doc.tf {
                if let Some(&q_freq) = qt.get(term) {
                    let idf = self.idf.get(term).copied().unwrap_or(1.0);
                    score += (q_freq * idf) * (tf * idf);
                }
            }
            score /= doc.norm * query_norm;

            let d_lower = doc.id.to_lowercase();
            if !d_lower.is_empty()
                && (q_lower.contains(&d_lower) || d_lower.contains(&q_lower)) {
                    score += 0.5;
                }
            if score > 0.0 {
                results.push((doc.id.clone(), score));
            }
        }
        results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        if top_k > 0 {
            results.truncate(top_k);
        }
        results
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn docs() -> Vec<Doc> {
        vec![
            Doc::build(
                "pr_review",
                "PR 代码审查与质量检查 查看 GitHub PR 变更、审查代码 diff、分析 bug、发表 review 评论代码审查 code review pull request diff",
            ),
            Doc::build(
                "db_analytics",
                "数据库分析 排查 postgres 慢查询、索引健康、表结构分析 database query slow query index performance",
            ),
            Doc::build(
                "github_issue",
                "GitHub issue 管理 创建 issue、回复评论、修改标签 issue management",
            ),
        ]
    }

    #[test]
    fn pr_query_ranks_pr_skill_first() {
        let mut m = Matcher::new();
        m.rebuild(docs());
        let ranked = m.rank("帮我审查这个 pull request 的代码 diff", 3);
        assert!(!ranked.is_empty());
        assert_eq!(ranked[0].0, "pr_review", "got {:?}", ranked);
        // weak single-char CJK ties must not drag in unrelated skills
        assert!(
            !ranked.iter().any(|(id, _)| id == "db_analytics"),
            "got {:?}",
            ranked
        );
    }

    #[test]
    fn db_query_ranks_db_skill_first() {
        let mut m = Matcher::new();
        m.rebuild(docs());
        let ranked = m.rank("postgres 慢查询排查 database slow query", 3);
        assert_eq!(ranked[0].0, "db_analytics", "got {:?}", ranked);
    }

    #[test]
    fn english_query_works() {
        let mut m = Matcher::new();
        m.rebuild(docs());
        let ranked = m.rank("how do I post a code review comment", 3);
        assert_eq!(ranked[0].0, "pr_review", "got {:?}", ranked);
    }

    #[test]
    fn empty_query_returns_everything() {
        let mut m = Matcher::new();
        m.rebuild(docs());
        assert_eq!(m.rank("   ", 0).len(), 3);
    }

    #[test]
    fn no_match_returns_empty() {
        let mut m = Matcher::new();
        m.rebuild(docs());
        assert!(m.rank("quantum physics of magnets", 3).is_empty());
    }
}