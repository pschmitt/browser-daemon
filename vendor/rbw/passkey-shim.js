/* MAIN world, no extension APIs. The fake WebAuthn credential a page gets from
   a vault-signed assertion, and the platform-authenticator answers; browser-daemon
   vendors this file, so keep it self-contained. Leaves `rbwPasskeyShim` on the
   global for the script that follows, which takes it and deletes it. */
(() => {
  const decode = value => {
    const text = atob(value.replace(/-/g, "+").replace(/_/g, "/"));
    return Uint8Array.from(text, c => c.charCodeAt(0)).buffer;
  };
  function credential(value, method) {
    const data = value.response;
    const response = Object.create(method === "create" ? AuthenticatorAttestationResponse.prototype : AuthenticatorAssertionResponse.prototype);
    for (const field of ["clientDataJSON", "attestationObject", "authenticatorData", "signature", "userHandle"]) {
      if (field in data) Object.defineProperty(response, field, { value: data[field] == null ? null : decode(data[field]), enumerable: true });
    }
    if (method === "create") {
      Object.defineProperties(response, {
        getTransports: { value: () => data.transports || ["internal"] },
        getPublicKey: { value: () => data.publicKey ? decode(data.publicKey) : null },
        getPublicKeyAlgorithm: { value: () => data.publicKeyAlgorithm || -7 },
        getAuthenticatorData: { value: () => decode(data.authenticatorData) },
      });
    }
    const result = Object.create(PublicKeyCredential.prototype);
    Object.defineProperties(result, {
      id: { value: value.id, enumerable: true }, rawId: { value: decode(value.rawId), enumerable: true },
      type: { value: "public-key", enumerable: true }, response: { value: response, enumerable: true },
      authenticatorAttachment: { value: "platform", enumerable: true },
      getClientExtensionResults: { writable: true, configurable: true, value: () => value.clientExtensionResults || {} },
      toJSON: { writable: true, configurable: true, value: () => value },
    });
    return result;
  }
  /* Answers a page's platform-authenticator checks: `available` is an async
     function saying whether a vault passkey can be offered here. */
  function patchPlatformAuthenticator(available) {
    for (const name of ["isUserVerifyingPlatformAuthenticatorAvailable", "isConditionalMediationAvailable"]) {
      const fallback = PublicKeyCredential[name]?.bind(PublicKeyCredential);
      if (fallback) Object.defineProperty(PublicKeyCredential, name, { configurable: true, value: async () => await available() || await fallback() });
    }
    const capabilities = PublicKeyCredential.getClientCapabilities?.bind(PublicKeyCredential);
    if (capabilities) Object.defineProperty(PublicKeyCredential, "getClientCapabilities", { configurable: true, value: async () => {
      const result = await capabilities();
      return await available() ? { ...result, userVerifyingPlatformAuthenticator: true, passkeyPlatformAuthenticator: true, conditionalGet: true } : result;
    } });
  }
  Object.defineProperty(globalThis, "rbwPasskeyShim", { configurable: true, value: { decode, credential, patchPlatformAuthenticator } });
})();
