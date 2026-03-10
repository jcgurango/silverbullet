/**
 * Simple JWT validation for the CRDT sidecar.
 * Uses HMAC-SHA256 with the shared SB_AUTH_SECRET.
 * When no secret is configured, all connections are allowed.
 */

const encoder = new TextEncoder();

export class Auth {
  private secret: string | undefined;
  private cryptoKey: CryptoKey | undefined;

  constructor(secret?: string) {
    this.secret = secret;
  }

  async init(): Promise<void> {
    if (!this.secret) return;
    this.cryptoKey = await crypto.subtle.importKey(
      "raw",
      encoder.encode(this.secret),
      { name: "HMAC", hash: "SHA-256" },
      false,
      ["sign", "verify"],
    );
  }

  /** Returns true if the token is valid (or no auth is configured) */
  async validateToken(token: string | null): Promise<boolean> {
    if (!this.secret || !this.cryptoKey) {
      // No auth configured, allow all
      return true;
    }

    if (!token) return false;

    try {
      const parts = token.split(".");
      if (parts.length !== 3) return false;

      const [headerB64, payloadB64, signatureB64] = parts;
      const data = encoder.encode(`${headerB64}.${payloadB64}`);
      const signature = base64UrlDecode(signatureB64).buffer as ArrayBuffer;

      const valid = await crypto.subtle.verify(
        "HMAC",
        this.cryptoKey,
        signature,
        data,
      );

      if (!valid) return false;

      // Check expiration
      const payload = JSON.parse(atob(payloadB64));
      if (payload.exp && payload.exp < Date.now() / 1000) {
        return false;
      }

      return true;
    } catch {
      return false;
    }
  }
}

function base64UrlDecode(str: string): Uint8Array {
  // Convert base64url to base64
  let base64 = str.replace(/-/g, "+").replace(/_/g, "/");
  while (base64.length % 4) base64 += "=";
  const binary = atob(base64);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) {
    bytes[i] = binary.charCodeAt(i);
  }
  return bytes;
}
