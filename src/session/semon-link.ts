/** Builds the pre-release Semon viewer URL in one place. */
export function semonSessionUrl(baseUrl: string, harness: string, sessionId: string): string {
  const id = encodeURIComponent(sessionId).replace(/[!'()*]/g, (character) => `%${character.charCodeAt(0).toString(16).toUpperCase()}`);
  return `${baseUrl.replace(/\/+$/, "")}/session/by-native/${harness}/${id}`;
}
