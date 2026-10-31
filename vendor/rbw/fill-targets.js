/* Isolated world, no extension APIs. Which input of a page is the username, the
   password and the one-time-code field of its login. The fill (fill.js), the
   autofill prompt (content.js) and browser-daemon, which vendors this file, all
   use it, so keep it self-contained. Leaves `rbwFillTargets` on the global. */
(() => {
  // Authenticator-code fields: autocomplete=one-time-code, or a name that says so
  // (signInMFA, ci-otpCode-0, totp, 2fa, ...).
  const isOtpField = el => ["text", "tel", "number", ""].includes(el.type) && !el.autocomplete.split(/\s+/).includes("username") && (el.autocomplete.split(/\s+/).includes("one-time-code") || /(^|[^a-z])(otp|totp|mfa|2fa|tfa|authenticator|one[\s_-]*time|verification[\s_-]*code|auth(entication)?[\s_-]*code)([^a-z]|$)/i.test([el.id, el.name, el.getAttribute("aria-label"), el.placeholder].filter(Boolean).join(" ").replace(/([a-z])([A-Z])/g, "$1 $2")));
  /* The login form's targets among the fillable `fields`. Throws when several
     password fields leave no choice. `scoped` is the form's own fields. */
  function loginTargets(fields) {
    // Masked integration credentials are not login passwords, even on a
    // matching site. Include associated labels (Rails often labels API keys).
    const isIntegrationSecret = el => {
      const text = [el.id, el.name, el.placeholder, el.getAttribute("aria-label"),
        ...Array.from(el.labels || [], label => label.textContent),
        ...(el.getAttribute("aria-labelledby") || "").split(/\s+/).filter(Boolean).map(id => document.getElementById(id)?.textContent)]
        .filter(Boolean).join(" ").replace(/([a-z])([A-Z])/g, "$1 $2");
      return /api[\s_-]*key|access[\s_-]*key|token|client[\s_-]*secret|private[\s_-]*key|secret[\s_-]*key/i.test(text);
    };
    const passwords = fields.filter(el => el.type === "password" && !el.autocomplete.split(/\s+/).includes("new-password") && !isIntegrationSecret(el));
    if (passwords.length > 1) throw new Error("Several password fields found. Select a login form.");
    const password = passwords[0];
    const isSearch = el => el.type === "search" || el.closest('[role="search"],form[action*="search"]') || /search|(^|[\s_-])q($|[\s_-])/i.test([el.id, el.name, el.autocomplete, el.getAttribute("aria-label"), el.placeholder].filter(Boolean).join(" "));
    const users = fields.filter(el => (!password?.form || el.form === password.form) && ["text", "email", "tel"].includes(el.type) && !isSearch(el) && !isOtpField(el));
    const recognizable = el => el.autocomplete?.split(/\s+/).includes("username") || el.type === "email" || /(^|[\s_-])(user(name)?|email|login|identifier|account)([\s_-]|$)/i.test([el.id, el.name, el.getAttribute("aria-label")].filter(Boolean).join(" "));
    // Generic text fields are only eligible inside the same real login form.
    // Username-only flows require explicit login semantics; never guess a search box.
    const usernameOnly = el => el.autocomplete?.split(/\s+/).includes("username") || /username|identifier|login/i.test([el.id, el.name].join(" ")) || (recognizable(el) && /login|sign.?in|auth/i.test([el.form?.id, el.form?.name, el.form?.getAttribute("action"), el.form?.getAttribute("aria-label")].filter(Boolean).join(" ")));
    const user = password ? users.find(recognizable) || (password.form ? users.filter(el => el.compareDocumentPosition(password) & Node.DOCUMENT_POSITION_FOLLOWING).at(-1) : null) : users.find(usernameOnly);
    const scoped = password?.form ? fields.filter(el => el.form === password.form) : fields;
    return { user, password, otp: fields.find(isOtpField), scoped };
  }
  Object.defineProperty(globalThis, "rbwFillTargets", { configurable: true, value: { isOtpField, loginTargets } });
})();
