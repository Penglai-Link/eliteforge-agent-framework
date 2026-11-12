:root { color-scheme: light dark; font-family: system-ui, sans-serif; }
body { margin: 0; color: var(--color-text-primary, #222); background: var(--color-bg-primary, #fafafa); }
main { max-width: 720px; margin: 8vh auto; padding: 32px; }
.eyebrow { font-size: 12px; letter-spacing: .08em; color: var(--color-text-secondary, #888); }
h1 { font-size: clamp(28px, 5vw, 48px); line-height: 1.2; margin-block: 24px 16px; }
p { line-height: 1.7; }
button { margin-top: 24px; padding: 12px 20px; font: inherit; cursor: pointer; color: inherit; background: transparent; border: 1px solid currentColor; border-radius: 8px; }
button:focus-visible { outline: 3px solid var(--color-primary, #4466cc); outline-offset: 4px; }
.context { margin-top: 48px; font-size: 12px; color: var(--color-text-secondary, #888); }
