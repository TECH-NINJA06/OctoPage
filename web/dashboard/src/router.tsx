// A small router on the History API: the service answers every page path with the
// dashboard, which then picks the view from the path.

import { useEffect, useState, type AnchorHTMLAttributes, type MouseEvent } from 'react';

const NAVIGATE = 'octopage:navigate';

export function navigate(to: string, { replace = false } = {}) {
  if (replace) history.replaceState(null, '', to);
  else history.pushState(null, '', to);
  window.dispatchEvent(new Event(NAVIGATE));
}

/** The current location, re-rendering on navigation. */
export function useLocation(): { path: string; query: URLSearchParams } {
  const read = () => ({ path: location.pathname, query: new URLSearchParams(location.search) });
  const [current, setCurrent] = useState(read);
  useEffect(() => {
    const update = () => setCurrent(read());
    window.addEventListener('popstate', update);
    window.addEventListener(NAVIGATE, update);
    return () => {
      window.removeEventListener('popstate', update);
      window.removeEventListener(NAVIGATE, update);
    };
  }, []);
  return current;
}

/** A link within the dashboard: no page load, but ordinary links for new tabs. */
export function Link({ to, onClick, ...rest }: { to: string } & AnchorHTMLAttributes<HTMLAnchorElement>) {
  const follow = (event: MouseEvent<HTMLAnchorElement>) => {
    onClick?.(event);
    if (event.defaultPrevented || event.button !== 0) return;
    if (event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
    event.preventDefault();
    navigate(to);
  };
  return <a href={to} onClick={follow} {...rest} />;
}
