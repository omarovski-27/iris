//! The deterministic, dependency-free baseline polisher.
//!
//! [`RulePolisher`] is the floor under everything else: it runs in microseconds,
//! needs no network, and is what Iris uses when there is no API key, when the
//! machine is offline, and whenever the LLM path blows its latency budget.
//!
//! It is deliberately timid. Every rule below was chosen because its worst case
//! is "left the text alone", never "said something the speaker did not say".

use std::time::{Duration, Instant};

use crate::polisher::{PolishFuture, Polisher};
use crate::types::{PolishRequest, PolishSource, Polished};

/// Standalone words removed as filler.
///
/// Only sounds that are never words in their own right. Notably absent, on
/// purpose: `like`, `so`, `well`, `right`, `actually`, `basically`, `I mean` —
/// each of them is load-bearing often enough that removing it risks changing a
/// sentence, which is exactly the trade this crate refuses to make.
pub const FILLER_WORDS: &[&str] = &[
    "um", "umm", "ummm", "uhm", "uh", "uhh", "uhhh", "erm", "mmm",
];

/// Multi-word filler, removed only when clearly parenthetical.
///
/// See [`RulePolisher`] for the delimiting rule; `you know` is a filler in
/// "it's hard, you know, to say" and a verb phrase in "you know the answer", and
/// the only reliable signal between them is punctuation.
///
/// `kind of like`, `sort of`, and `I mean` are deliberately absent: each carries
/// real meaning often enough ("it's kind of like a cache") that no punctuation
/// rule can tell the readings apart.
pub const FILLER_PHRASES: &[&[&str]] = &[&["you", "know"]];

/// Words where an immediate verbatim repeat is always a stutter.
///
/// Deliberately excludes `that`, `had`, `is`, and `so`: "the thing that that guy
/// said", "he had had enough", and "it was so so close" are all real English.
pub const STUTTER_WORDS: &[&str] = &[
    "i", "a", "an", "the", "and", "of", "to", "in", "it", "we", "they", "but", "or", "for", "on",
    "with", "this", "at", "from",
];

/// Words whose trailing period does not end a sentence.
const ABBREVIATIONS: &[&str] = &[
    "e.g", "i.e", "etc", "vs", "mr", "mrs", "ms", "dr", "prof", "st", "jr", "sr", "approx", "fig",
    "inc", "ltd", "cf", "al",
];

/// Characters treated as leading punctuation on a token.
const OPENERS: &str = "([{\"'\u{201c}\u{2018}\u{00bf}\u{00a1}";
/// Characters treated as trailing punctuation on a token.
const CLOSERS: &str = ".,!?;:)]}\"'\u{201d}\u{2019}\u{2026}";
/// Trailing punctuation that already terminates an utterance.
const TERMINATORS: &str = ".!?:;\u{2026}";

/// Which rules [`RulePolisher`] applies.
///
/// Every flag can be turned off independently; turning them all off leaves a
/// pure whitespace normaliser, which is the most conservative useful polisher.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct RuleConfig {
    /// Remove standalone filler sounds ([`FILLER_WORDS`]).
    pub remove_fillers: bool,
    /// Remove parenthetical filler phrases ([`FILLER_PHRASES`]).
    pub remove_filler_phrases: bool,
    /// Collapse doubled function words ([`STUTTER_WORDS`]).
    pub collapse_stutters: bool,
    /// Capitalise the first word of every sentence.
    pub capitalize_sentences: bool,
    /// Capitalise the pronoun `i` and its contractions.
    pub capitalize_pronoun_i: bool,
    /// Give the utterance a terminal `.` when it has none.
    pub ensure_terminal_punctuation: bool,
}

impl Default for RuleConfig {
    fn default() -> Self {
        Self {
            remove_fillers: true,
            remove_filler_phrases: true,
            collapse_stutters: true,
            capitalize_sentences: true,
            capitalize_pronoun_i: true,
            ensure_terminal_punctuation: true,
        }
    }
}

impl RuleConfig {
    /// Whitespace normalisation only: no removals, no casing, no punctuation.
    pub fn minimal() -> Self {
        Self {
            remove_fillers: false,
            remove_filler_phrases: false,
            collapse_stutters: false,
            capitalize_sentences: false,
            capitalize_pronoun_i: false,
            ensure_terminal_punctuation: false,
        }
    }
}

/// Instant, offline, deterministic transcript cleanup.
///
/// # What it does
///
/// 1. **Whitespace** — collapses runs of spaces and tabs, trims, and keeps at
///    most one blank line between paragraphs.
/// 2. **Filler sounds** — drops standalone [`FILLER_WORDS`]. Punctuation the
///    filler was carrying moves back onto the previous word, so
///    `"that's it, um."` becomes `"That's it."` rather than `"That's it,"`.
/// 3. **Filler phrases** — drops [`FILLER_PHRASES`], but only where the phrase is
///    *delimited on both sides* (a comma or the utterance boundary each side).
///    `"it's hard, you know, to say"` loses it; `"you know the answer"` keeps it.
/// 4. **Stutters** — collapses an immediately repeated [`STUTTER_WORDS`] entry.
/// 5. **Casing** — capitalises the first word of each sentence and the pronoun
///    `i`. Never *lowercases* anything, and never touches a protected token.
/// 6. **Terminal punctuation** — appends `.` when the utterance ends bare, and
///    promotes a stray trailing comma to a period.
///
/// # What it never does
///
/// - Reorder, substitute, or add words.
/// - Touch a **protected token**: anything containing a digit, `://`, `@`, `/`,
///   `\`, `` ` ``, `_`, `::`, `#`, `$`, an internal capital, or an internal dot.
///   URLs, emails, paths, versions, `snake_case`, `camelCase`, `ALLCAPS`, and
///   `fn_call()` all pass through byte-for-byte.
/// - Append a period after a URL, email, path, or code span, where a trailing
///   dot would break the token when copied.
/// - Return empty output. If the rules would erase everything (an utterance that
///   is nothing but filler), the whitespace-normalised input is returned instead.
///
/// # Cost
///
/// One pass, a few allocations, no I/O. Microseconds for a normal utterance —
/// which is why [`FallbackPolisher`](crate::FallbackPolisher) can afford to run
/// it unconditionally on every request as insurance.
///
/// ```
/// use iris_polish::{PolishRequest, Polisher, RulePolisher};
///
/// # async fn demo() {
/// let out = RulePolisher::default()
///     .polish(&PolishRequest::new("um so uh i think the the fix works"))
///     .await
///     .unwrap();
/// assert_eq!(out.text, "So I think the fix works.");
/// # }
/// ```
#[derive(Clone, Copy, Debug, Default)]
pub struct RulePolisher {
    config: RuleConfig,
}

impl RulePolisher {
    /// A polisher with every rule enabled.
    pub fn new() -> Self {
        Self::default()
    }

    /// A polisher with a specific rule set.
    pub fn with_config(config: RuleConfig) -> Self {
        Self { config }
    }

    /// The rules in force.
    pub fn config(&self) -> &RuleConfig {
        &self.config
    }

    /// Clean `text` synchronously.
    ///
    /// [`Polisher::polish`] is a thin async wrapper over this; nothing here
    /// blocks or waits, so callers on a non-async path can use it directly.
    pub fn polish_str(&self, text: &str) -> String {
        let paragraphs = split_paragraphs(text);
        if paragraphs.is_empty() {
            return text.trim().to_string();
        }

        let cleaned: Vec<String> = paragraphs
            .iter()
            .map(|p| self.polish_paragraph(p))
            .filter(|p| !p.is_empty())
            .collect();

        let out = cleaned.join("\n\n");

        // Safety net: the rules must never delete an utterance outright. An
        // input of pure filler comes back normalised but intact.
        if out.trim().is_empty() && !text.trim().is_empty() {
            return paragraphs.join("\n\n");
        }
        out
    }

    fn polish_paragraph(&self, paragraph: &str) -> String {
        let mut tokens = tokenize(paragraph);
        if tokens.is_empty() {
            return String::new();
        }

        if self.config.remove_fillers {
            tokens = remove_filler_words(tokens);
        }
        if self.config.remove_filler_phrases {
            tokens = remove_filler_phrases(tokens);
        }
        if self.config.collapse_stutters {
            tokens = collapse_stutters(tokens);
        }
        if self.config.capitalize_pronoun_i {
            capitalize_pronoun_i(&mut tokens);
        }
        if self.config.capitalize_sentences {
            capitalize_sentences(&mut tokens);
        }
        if self.config.ensure_terminal_punctuation {
            ensure_terminal_punctuation(&mut tokens);
        }

        render(&tokens)
    }
}

impl Polisher for RulePolisher {
    fn name(&self) -> &'static str {
        "rule"
    }

    fn polish<'a>(&'a self, request: &'a PolishRequest) -> PolishFuture<'a> {
        Box::pin(async move {
            let started = Instant::now();
            let text = self.polish_str(&request.text);
            let source = if text == request.text {
                PolishSource::Unchanged
            } else {
                PolishSource::Rule
            };
            Ok(Polished::new(text, source, started.elapsed()))
        })
    }

    fn expected_latency(&self) -> Option<Duration> {
        Some(Duration::from_micros(50))
    }
}

// ---------------------------------------------------------------------------
// Tokens
// ---------------------------------------------------------------------------

/// One whitespace-delimited chunk, split into punctuation and content.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Token {
    prefix: String,
    word: String,
    suffix: String,
    /// Content the rules must not touch: identifiers, URLs, numbers, ...
    protected: bool,
}

impl Token {
    fn lower(&self) -> String {
        self.word.to_lowercase()
    }
}

/// Split into paragraphs, collapsing intra-paragraph whitespace.
fn split_paragraphs(text: &str) -> Vec<String> {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut paragraphs = Vec::new();
    let mut current: Vec<String> = Vec::new();

    for line in normalized.split('\n') {
        if line.trim().is_empty() {
            if !current.is_empty() {
                paragraphs.push(current.join(" "));
                current.clear();
            }
        } else {
            current.push(line.split_whitespace().collect::<Vec<_>>().join(" "));
        }
    }
    if !current.is_empty() {
        paragraphs.push(current.join(" "));
    }
    paragraphs.retain(|p| !p.trim().is_empty());
    paragraphs
}

fn tokenize(paragraph: &str) -> Vec<Token> {
    let mut tokens: Vec<Token> = Vec::new();

    for chunk in paragraph.split_whitespace() {
        let token = split_chunk(chunk);

        // A chunk that is nothing but punctuation (ASR sometimes emits " , ")
        // belongs to the word before it.
        if token.word.is_empty() {
            let orphan = format!("{}{}", token.prefix, token.suffix);
            match tokens.last_mut() {
                Some(prev) => prev.suffix.push_str(&orphan),
                None => tokens.push(Token {
                    prefix: orphan,
                    word: String::new(),
                    suffix: String::new(),
                    protected: true,
                }),
            }
            continue;
        }
        tokens.push(token);
    }

    for token in &mut tokens {
        normalize_suffix(&mut token.suffix);
    }
    tokens
}

fn split_chunk(chunk: &str) -> Token {
    let chars: Vec<char> = chunk.chars().collect();

    let mut start = 0;
    while start < chars.len() && OPENERS.contains(chars[start]) {
        start += 1;
    }
    let mut end = chars.len();
    while end > start && CLOSERS.contains(chars[end - 1]) {
        end -= 1;
    }

    let prefix: String = chars[..start].iter().collect();
    let word: String = chars[start..end].iter().collect();
    let suffix: String = chars[end..].iter().collect();
    let protected = is_protected(&word);

    Token {
        prefix,
        word,
        suffix,
        protected,
    }
}

/// Content the rules refuse to modify.
///
/// The test is intentionally broad. A false positive costs a missed
/// capitalisation; a false negative corrupts an identifier the user dictated on
/// purpose. Only one of those is recoverable by the reader.
fn is_protected(word: &str) -> bool {
    if word.is_empty() {
        return false;
    }
    if word.contains("://")
        || word.contains('@')
        || word.contains('`')
        || word.contains('_')
        || word.contains('/')
        || word.contains('\\')
        || word.contains('#')
        || word.contains('$')
        || word.contains("::")
        || word.ends_with("()")
    {
        return true;
    }
    if word.chars().any(|c| c.is_ascii_digit()) {
        return true;
    }
    // Internal capital: GitHub, iPhone, ALLCAPS, wgpuDevice.
    if word.chars().skip(1).any(char::is_uppercase) {
        return true;
    }
    // Internal dot: example.com, e.g, config.toml.
    if word.contains('.') {
        return true;
    }
    false
}

/// Tokens where a trailing period would corrupt the token if copied.
fn is_punctuation_sensitive(word: &str) -> bool {
    word.contains("://")
        || word.contains('@')
        || word.contains('`')
        || word.contains('/')
        || word.contains('\\')
        || word.ends_with("()")
}

/// Drop commas next to stronger punctuation and collapse repeats.
fn normalize_suffix(suffix: &mut String) {
    if suffix.is_empty() {
        return;
    }
    let has_strong = suffix.chars().any(|c| ".!?;:".contains(c));
    let mut out = String::with_capacity(suffix.len());
    let mut last: Option<char> = None;
    for c in suffix.chars() {
        if c == ',' && has_strong {
            continue;
        }
        if c == ',' && last == Some(',') {
            continue;
        }
        out.push(c);
        last = Some(c);
    }
    *suffix = out;
}

fn strong_punct(suffix: &str) -> Option<char> {
    suffix.chars().find(|c| ".!?".contains(*c))
}

/// Repair the punctuation left behind when filler is deleted.
///
/// Two cases, both about punctuation that belonged to the filler rather than to
/// the sentence:
///
/// - The filler carried the sentence's terminator (`"that's it, um."`) — move it
///   back onto the preceding word so the sentence still ends.
/// - The filler was fenced by commas (`"the build is, um, still running"`) — the
///   opening comma existed only to fence it, so it goes too.
fn absorb_removal(out: &mut [Token], removed: &Token) {
    if let Some(punct) = strong_punct(&removed.suffix) {
        if let Some(prev) = out.last_mut() {
            if prev.suffix.is_empty() || prev.suffix == "," {
                prev.suffix = punct.to_string();
            }
        }
        return;
    }
    if removed.suffix.contains(',') {
        if let Some(prev) = out.last_mut() {
            if prev.suffix == "," {
                prev.suffix.clear();
            }
        }
    }
}

fn remove_filler_words(tokens: Vec<Token>) -> Vec<Token> {
    let mut out: Vec<Token> = Vec::with_capacity(tokens.len());
    for token in tokens {
        let is_filler = !token.protected
            && token.prefix.is_empty()
            && FILLER_WORDS.contains(&token.lower().as_str());
        if is_filler {
            absorb_removal(&mut out, &token);
            continue;
        }
        out.push(token);
    }
    out
}

/// Remove a filler phrase only when it is parenthetical.
///
/// The phrase must be delimited on *both* sides — by a comma or by the edge of
/// the utterance. That is the only signal that separates the filler reading of
/// "you know" from the literal one, and when the signal is absent the phrase
/// stays. Whisper-class engines punctuate, so this fires on real transcripts;
/// on an unpunctuated one it correctly does nothing.
fn remove_filler_phrases(tokens: Vec<Token>) -> Vec<Token> {
    let mut tokens = tokens;
    for phrase in FILLER_PHRASES {
        tokens = remove_one_phrase(tokens, phrase);
    }
    tokens
}

fn remove_one_phrase(tokens: Vec<Token>, phrase: &[&str]) -> Vec<Token> {
    let len = phrase.len();
    if tokens.len() < len {
        return tokens;
    }

    let mut out: Vec<Token> = Vec::with_capacity(tokens.len());
    let mut i = 0;
    while i < tokens.len() {
        let matches = i + len <= tokens.len()
            && tokens[i..i + len]
                .iter()
                .zip(phrase)
                .all(|(t, w)| !t.protected && t.lower() == *w)
            // Interior punctuation would mean this is not one phrase.
            && tokens[i..i + len - 1]
                .iter()
                .all(|t| t.suffix.is_empty() && t.prefix.is_empty());

        if matches {
            let last = &tokens[i + len - 1];
            let opened = i == 0 || out.last().is_some_and(|p: &Token| p.suffix.contains(','));
            let closed = i + len == tokens.len() || last.suffix.contains(',');

            if opened && closed {
                absorb_removal(&mut out, last);
                // A trailing filler leaves the comma that fenced it dangling.
                if i + len == tokens.len() {
                    if let Some(prev) = out.last_mut() {
                        if prev.suffix == "," {
                            prev.suffix.clear();
                        }
                    }
                }
                i += len;
                continue;
            }
        }
        out.push(tokens[i].clone());
        i += 1;
    }
    out
}

fn collapse_stutters(tokens: Vec<Token>) -> Vec<Token> {
    let mut out: Vec<Token> = Vec::with_capacity(tokens.len());
    for token in tokens {
        let repeats = out.last().is_some_and(|prev: &Token| {
            !prev.protected
                && !token.protected
                && prev.suffix.is_empty()
                && prev.prefix.is_empty()
                && token.prefix.is_empty()
                && prev.lower() == token.lower()
                && STUTTER_WORDS.contains(&token.lower().as_str())
        });
        if repeats {
            // Keep the later copy: it carries any punctuation that followed.
            out.pop();
        }
        out.push(token);
    }
    out
}

fn capitalize_pronoun_i(tokens: &mut [Token]) {
    for token in tokens.iter_mut() {
        if token.protected {
            continue;
        }
        let lower = token.lower();
        let is_pronoun = matches!(
            lower.as_str(),
            "i" | "i'm"
                | "i've"
                | "i'll"
                | "i'd"
                | "i\u{2019}m"
                | "i\u{2019}ve"
                | "i\u{2019}ll"
                | "i\u{2019}d"
        );
        if is_pronoun && token.word.starts_with('i') {
            token.word.replace_range(..1, "I");
        }
    }
}

/// True when this token's trailing punctuation really ends a sentence.
fn ends_sentence(token: &Token) -> bool {
    if !token.suffix.chars().any(|c| ".!?".contains(c)) {
        return false;
    }
    if token.word.contains('.') && !token.suffix.chars().any(|c| "!?".contains(c)) {
        return false;
    }
    let lower = token.lower();
    if ABBREVIATIONS.contains(&lower.as_str()) {
        return false;
    }
    // A lone initial: "J. Smith".
    if token.word.chars().count() == 1 && token.word.chars().all(char::is_alphabetic) {
        return false;
    }
    true
}

fn capitalize_sentences(tokens: &mut [Token]) {
    let mut at_start = true;
    for token in tokens.iter_mut() {
        if at_start && !token.protected {
            capitalize_first(&mut token.word);
        }
        if !token.word.is_empty() {
            at_start = ends_sentence(token);
        }
    }
}

fn capitalize_first(word: &mut String) {
    let mut chars = word.chars();
    let Some(first) = chars.next() else {
        return;
    };
    if !first.is_lowercase() {
        return;
    }
    let upper: String = first.to_uppercase().collect();
    let rest: String = chars.collect();
    *word = upper + &rest;
}

fn ensure_terminal_punctuation(tokens: &mut [Token]) {
    let Some(last) = tokens.last_mut() else {
        return;
    };
    if last.word.is_empty() {
        return;
    }
    if is_punctuation_sensitive(&last.word) {
        return;
    }
    // A trailing comma at the end of an utterance is always a mistake.
    while last.suffix.ends_with(',') {
        last.suffix.pop();
    }
    let terminated = last.suffix.chars().any(|c| TERMINATORS.contains(c));
    if !terminated {
        last.suffix.push('.');
    }
}

fn render(tokens: &[Token]) -> String {
    let mut out = String::new();
    for token in tokens {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&token.prefix);
        out.push_str(&token.word);
        out.push_str(&token.suffix);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn polish(text: &str) -> String {
        RulePolisher::default().polish_str(text)
    }

    // -- whitespace ---------------------------------------------------------

    #[test]
    fn collapses_repeated_whitespace() {
        assert_eq!(polish("hello    there\t\tfriend"), "Hello there friend.");
    }

    #[test]
    fn trims_and_keeps_paragraph_breaks() {
        assert_eq!(
            polish("  first thought\n\n\n  second thought  "),
            "First thought.\n\nSecond thought."
        );
    }

    #[test]
    fn single_newline_is_just_a_space() {
        assert_eq!(polish("one line\nsame sentence"), "One line same sentence.");
    }

    #[test]
    fn crlf_is_handled() {
        assert_eq!(polish("a line\r\n\r\nnext one"), "A line.\n\nNext one.");
    }

    #[test]
    fn empty_input_stays_empty() {
        assert_eq!(polish(""), "");
        assert_eq!(polish("   \n  "), "");
    }

    // -- fillers ------------------------------------------------------------

    #[test]
    fn strips_leading_filler_and_its_comma() {
        assert_eq!(polish("um, I think so"), "I think so.");
        assert_eq!(polish("Uh I think so"), "I think so.");
    }

    #[test]
    fn strips_interior_filler_and_the_commas_that_fenced_it() {
        assert_eq!(
            polish("the build is, um, still running"),
            "The build is still running."
        );
    }

    #[test]
    fn keeps_a_comma_that_was_not_just_a_fence() {
        // Only the filler's own comma goes; the clause comma stays.
        assert_eq!(
            polish("when it lands, um send me a link"),
            "When it lands, send me a link."
        );
    }

    #[test]
    fn filler_donates_its_terminal_punctuation() {
        assert_eq!(polish("that is all um."), "That is all.");
        assert_eq!(polish("is that right uh?"), "Is that right?");
    }

    #[test]
    fn filler_matching_is_whole_token_only() {
        assert_eq!(polish("umbrella and uhuru"), "Umbrella and uhuru.");
    }

    #[test]
    fn filler_matching_is_case_insensitive() {
        assert_eq!(polish("Um, fine"), "Fine.");
        assert_eq!(polish("that is, Uh, done"), "That is done.");
    }

    #[test]
    fn all_caps_is_treated_as_an_acronym_not_filler() {
        // ALLCAPS is protected across the board, so a shouted "UM" survives.
        // No engine emits that, and the alternative — poking holes in acronym
        // protection — costs more than it saves.
        assert_eq!(polish("UM, fine"), "UM, fine.");
    }

    #[test]
    fn ambiguous_words_are_never_treated_as_filler() {
        assert_eq!(polish("I like it so well"), "I like it so well.");
        assert_eq!(polish("right, I mean that"), "Right, I mean that.");
        assert_eq!(
            polish("basically actually true"),
            "Basically actually true."
        );
    }

    #[test]
    fn pure_filler_input_is_returned_intact() {
        // Erasing the utterance is never an acceptable outcome.
        assert_eq!(polish("um"), "um");
        assert_eq!(polish("um uh umm"), "um uh umm");
    }

    // -- filler phrases -----------------------------------------------------

    #[test]
    fn removes_delimited_you_know() {
        assert_eq!(polish("so, you know, I fixed it"), "So I fixed it.");
    }

    #[test]
    fn removes_you_know_at_utterance_start() {
        assert_eq!(polish("you know, it works"), "It works.");
    }

    #[test]
    fn removes_trailing_you_know_and_keeps_the_period() {
        assert_eq!(polish("it is tricky, you know."), "It is tricky.");
    }

    #[test]
    fn keeps_you_know_when_it_is_a_verb_phrase() {
        assert_eq!(polish("you know the answer"), "You know the answer.");
        assert_eq!(
            polish("I think you know what happened"),
            "I think you know what happened."
        );
    }

    #[test]
    fn keeps_undelimited_you_know() {
        // No comma on either side: no evidence it is filler.
        assert_eq!(
            polish("it is hard you know to say"),
            "It is hard you know to say."
        );
    }

    // -- stutters -----------------------------------------------------------

    #[test]
    fn collapses_doubled_function_words() {
        assert_eq!(polish("the the fix landed"), "The fix landed.");
        assert_eq!(polish("I I tried that"), "I tried that.");
        assert_eq!(
            polish("we we should go to to lunch"),
            "We should go to lunch."
        );
    }

    #[test]
    fn keeps_legitimate_doubles() {
        assert_eq!(
            polish("the thing that that guy said"),
            "The thing that that guy said."
        );
        assert_eq!(polish("he had had enough"), "He had had enough.");
        assert_eq!(polish("it was so so close"), "It was so so close.");
    }

    #[test]
    fn does_not_collapse_across_punctuation() {
        assert_eq!(polish("I know it, it broke"), "I know it, it broke.");
    }

    // -- casing -------------------------------------------------------------

    #[test]
    fn capitalizes_each_sentence() {
        assert_eq!(
            polish("it works. try it now! does it? yes"),
            "It works. Try it now! Does it? Yes."
        );
    }

    #[test]
    fn capitalizes_the_pronoun_i() {
        assert_eq!(polish("then i left"), "Then I left.");
        assert_eq!(
            polish("i'm done and i've checked"),
            "I'm done and I've checked."
        );
    }

    #[test]
    fn never_lowercases_anything() {
        assert_eq!(polish("NASA called Bob"), "NASA called Bob.");
    }

    #[test]
    fn does_not_split_sentences_on_abbreviations() {
        assert_eq!(polish("use etc. sparingly"), "Use etc. sparingly.");
        assert_eq!(polish("ask dr. patel"), "Ask dr. patel.");
    }

    #[test]
    fn capitalizes_after_bare_no() {
        assert_eq!(polish("no. they left"), "No. They left.");
    }

    #[test]
    fn does_not_split_sentences_on_dotted_acronyms() {
        assert_eq!(polish("the U.S. economy"), "The U.S. economy.");
        assert_eq!(polish("a Ph.D. in physics"), "A Ph.D. in physics.");
    }

    #[test]
    fn does_not_split_sentences_on_initials() {
        assert_eq!(polish("it was j. smith"), "It was j. smith.");
    }

    // -- protected tokens ---------------------------------------------------

    #[test]
    fn preserves_urls_verbatim_and_adds_no_trailing_dot() {
        assert_eq!(
            polish("see https://example.com/a_b?x=1"),
            "See https://example.com/a_b?x=1"
        );
    }

    #[test]
    fn preserves_emails() {
        assert_eq!(
            polish("mail ops@example.com about it"),
            "Mail ops@example.com about it."
        );
    }

    #[test]
    fn preserves_code_identifiers_and_casing() {
        assert_eq!(
            polish("call polish_str on RulePolisher"),
            "Call polish_str on RulePolisher."
        );
        assert_eq!(polish("iPhone shipped"), "iPhone shipped.");
        assert_eq!(polish("run cargo test now"), "Run cargo test now.");
    }

    #[test]
    fn preserves_numbers_units_and_versions() {
        assert_eq!(
            polish("it took 42.5 ms on v1.2.3"),
            "It took 42.5 ms on v1.2.3."
        );
    }

    #[test]
    fn preserves_paths() {
        assert_eq!(polish("open src/main.rs"), "Open src/main.rs");
    }

    #[test]
    fn protected_token_at_sentence_start_is_not_recased() {
        assert_eq!(
            polish("done. wgpu_device failed"),
            "Done. wgpu_device failed."
        );
    }

    // -- terminal punctuation ----------------------------------------------

    #[test]
    fn appends_a_period_when_missing() {
        assert_eq!(polish("this is fine"), "This is fine.");
    }

    #[test]
    fn does_not_double_terminal_punctuation() {
        assert_eq!(polish("this is fine."), "This is fine.");
        assert_eq!(polish("really?"), "Really?");
        assert_eq!(polish("wow!"), "Wow!");
        assert_eq!(polish("as follows:"), "As follows:");
    }

    #[test]
    fn promotes_a_stray_trailing_comma() {
        assert_eq!(polish("and then we shipped,"), "And then we shipped.");
    }

    #[test]
    fn keeps_ellipsis() {
        assert_eq!(polish("well\u{2026}"), "Well\u{2026}");
    }

    // -- punctuation hygiene ------------------------------------------------

    #[test]
    fn reattaches_floating_punctuation() {
        assert_eq!(polish("hello , world"), "Hello, world.");
    }

    #[test]
    fn collapses_stacked_commas() {
        assert_eq!(polish("wait,, no"), "Wait, no.");
    }

    #[test]
    fn drops_a_comma_that_precedes_a_period() {
        assert_eq!(polish("done,."), "Done.");
    }

    #[test]
    fn keeps_quotes_and_brackets() {
        assert_eq!(polish("he said \"hello\""), "He said \"hello\".");
        assert_eq!(polish("(an aside) then more"), "(An aside) then more.");
    }

    // -- combined -----------------------------------------------------------

    #[test]
    fn realistic_utterance() {
        let raw = "um so   i was thinking, you know, we should uh ship the the fix \
                   to src/main.rs before 5 pm";
        assert_eq!(
            polish(raw),
            "So I was thinking we should ship the fix to src/main.rs before 5 pm."
        );
    }

    #[test]
    fn is_idempotent_on_its_own_output() {
        let raw = "um, so i think the the build at https://ci.example.com/job/7 is uh broken";
        let once = polish(raw);
        assert_eq!(polish(&once), once, "second pass changed the text");
    }

    // -- config -------------------------------------------------------------

    #[test]
    fn minimal_config_only_normalizes_whitespace() {
        let p = RulePolisher::with_config(RuleConfig::minimal());
        assert_eq!(p.polish_str("um   so  i went"), "um so i went");
    }

    #[test]
    fn rules_can_be_disabled_individually() {
        let config = RuleConfig {
            remove_fillers: false,
            ..RuleConfig::default()
        };
        let p = RulePolisher::with_config(config);
        assert_eq!(p.polish_str("um i went"), "Um I went.");
    }

    // -- async surface ------------------------------------------------------

    #[test]
    fn reports_unchanged_when_nothing_to_do() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let p = RulePolisher::default();
        let out = rt
            .block_on(p.polish(&PolishRequest::new("This is already clean.")))
            .unwrap();
        assert_eq!(out.source, PolishSource::Unchanged);
        assert_eq!(out.text, "This is already clean.");
    }

    #[test]
    fn reports_rule_source_and_a_duration_when_it_edits() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let p = RulePolisher::default();
        let out = rt
            .block_on(p.polish(&PolishRequest::new("um it edited this")))
            .unwrap();
        assert_eq!(out.source, PolishSource::Rule);
        assert_eq!(out.text, "It edited this.");
        assert!(!out.is_fallback());
        assert!(out.duration < Duration::from_millis(50));
    }

    // -- robustness ---------------------------------------------------------

    #[test]
    fn survives_odd_input() {
        for raw in [
            ".",
            ",",
            "\u{2014}",
            "a",
            "\u{4f60}\u{597d}",
            "?!?!",
            "( ) [ ]",
            "\"\"",
            "um,",
        ] {
            let out = polish(raw);
            assert!(!out.contains("  "), "double space in {out:?}");
        }
    }

    #[test]
    fn non_ascii_text_is_capitalized_correctly() {
        assert_eq!(polish("\u{e9}t\u{e9} was warm"), "\u{c9}t\u{e9} was warm.");
    }
}
