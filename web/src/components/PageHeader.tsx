import type { ReactNode } from "react";

export function PageHeader({
  title,
  sub,
  openNav,
  children,
}: {
  title: ReactNode;
  sub?: ReactNode;
  openNav: () => void;
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
    </header>
  );
}
