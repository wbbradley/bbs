import type { ReactNode } from "react";

export function PageHeader({
  title,
  sub,
  openNav,
  controls,
  children,
}: {
  title: ReactNode;
  sub?: ReactNode;
  openNav: () => void;
  /** Controls on their own row below the title. */
  controls?: ReactNode;
  children?: ReactNode;
}) {
  return (
    <header className="page-header">
      <button className="menu-button" onClick={(e) => (e.stopPropagation(), openNav())} aria-label="Menu">
        ☰
      </button>
      <h1>{title}</h1>
      {sub && <span className="sub">{sub}</span>}
      <span className="spacer" />
      {children}
      {controls && <div className="page-controls">{controls}</div>}
    </header>
  );
}
