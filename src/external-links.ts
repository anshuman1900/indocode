/** Opens external web links from Indocode's WebViews in the operating system browser. */
export function mountExternalLinks(
  document: Document,
  openExternalUrl: (url: string) => Promise<unknown>,
  onError: (error: unknown) => void,
): () => void {
  const handle = (event: MouseEvent) => {
    if (event.defaultPrevented) return;
    if (event.type === 'click' ? event.button !== 0 : event.button !== 1) return;

    const anchor = event.composedPath().find((target): target is HTMLAnchorElement => target instanceof HTMLAnchorElement);
    if (!anchor?.hasAttribute('href') || anchor.hasAttribute('download')) return;

    let url: URL;
    try { url = new URL(anchor.href, document.baseURI); }
    catch { return; }
    if (!['http:', 'https:'].includes(url.protocol) || !url.host) return;
    if (url.origin === document.location.origin && anchor.target.toLowerCase() !== '_blank') return;

    event.preventDefault();
    event.stopPropagation();
    try { void openExternalUrl(url.href).catch(onError); }
    catch (error) { onError(error); }
  };

  document.addEventListener('click', handle, true);
  document.addEventListener('auxclick', handle, true);
  return () => {
    document.removeEventListener('click', handle, true);
    document.removeEventListener('auxclick', handle, true);
  };
}
