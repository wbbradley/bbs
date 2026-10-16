#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["numpy>=2,<3", "contourpy>=1.3,<2", "shapely>=2,<3", "cairosvg>=2.7,<3"]
# ///
"""Draw BBS memberships with Bubble Sets; run with `uv run scripts/bubble_sets.py`."""

import argparse
import colorsys
import heapq
import itertools
import json
import math
import subprocess
import sys
from dataclasses import dataclass
from html import escape
from pathlib import Path

import contourpy
import numpy as np
from shapely.geometry import LineString, Polygon, box
from shapely.ops import unary_union
from shapely.prepared import prep


@dataclass(frozen=True)
class Node:
    name: str
    x: float
    y: float
    width: float
    height: float = 64

    @property
    def center(self):
        return (self.x, self.y)

    def bounds(self, padding=0):
        return (self.x - self.width / 2 - padding,
                self.y - self.height / 2 - padding,
                self.x + self.width / 2 + padding,
                self.y + self.height / 2 + padding)

    def shape(self, padding=0):
        return box(*self.bounds(padding))


@dataclass
class Bubble:
    name: str
    path: str
    shape: Polygon


def normalize(data):
    """JSON schema: {groups: {name: [members]}, agents?: [names], positions?: {...}}."""
    if not isinstance(data, dict) or not isinstance(data.get("groups"), dict):
        raise TypeError("input must be an object with a 'groups' object")

    def names(values):
        if not isinstance(values, list) or any(
            not isinstance(v, str) or not v.strip() or any(ord(c) < 32 for c in v)
            for v in values
        ):
            raise ValueError("names must be arrays of nonempty strings without control characters")
        return sorted(set(values))

    groups = {}
    for group, members in data["groups"].items():
        names([group])
        groups[group] = names(members)
    groups = dict(sorted(groups.items()))
    agents = sorted(set(names(data.get("agents", []))).union(*map(set, groups.values())))
    positions = data.get("positions")
    if positions is not None:
        if not isinstance(positions, dict) or set(positions) != set(agents):
            raise ValueError("positions must provide [x, y] for every agent, and no others")
        for value in positions.values():
            if (not isinstance(value, list) or len(value) != 2
                    or any(type(v) not in (int, float) or not math.isfinite(v) for v in value)):
                raise ValueError("positions must contain finite numeric [x, y] coordinates")
    return {"agents": agents, "groups": groups, **({"positions": positions} if positions is not None else {})}


def command(args):
    try:
        return subprocess.run(args, check=True, capture_output=True, text=True, timeout=60).stdout
    except FileNotFoundError as error:
        raise ValueError(f"{args[0]} is not installed or not on PATH") from error
    except subprocess.CalledProcessError as error:
        raise ValueError(f"{args[0]} failed: {error.stderr.strip()}") from error
    except subprocess.TimeoutExpired as error:
        raise ValueError(f"{args[0]} timed out after 60 seconds") from error


def read_bbs(bbs="bbs"):
    """Use the public CLI, respecting its server and caller configuration."""
    groups = {}
    for line in command([bbs, "group", "list", "--json"]).splitlines():
        try:
            row = json.loads(line)
            groups[row["name"]] = list(row["members"])
        except (ValueError, TypeError, KeyError) as error:
            raise ValueError(f"unrecognized bbs group list row: {line!r}") from error
    rows = command([bbs, "agents"]).splitlines()
    if not rows or rows[0].split()[:2] != ["NAME", "KIND"]:
        raise ValueError("unrecognized bbs agents header")
    agents = []
    for row in rows[1:]:
        fields = row.split()
        if len(fields) < 2 or fields[1] not in {"agent", "human", "retired"}:
            raise ValueError(f"unrecognized bbs agents row: {row!r}")
        if fields[1] == "agent":
            agents.append(fields[0])
    # Memberships are authoritative: an agent can join between these two reads.
    return normalize({"groups": groups, "agents": agents})


def layout(data, root=None):
    """Graphviz places a bipartite graph with invisible group nodes as anchors."""
    agents, groups = data["agents"], data["groups"]
    if root is not None and root not in groups:
        raise ValueError(f"unknown root group: {root}")
    widths = {name: max(108, 10 * len(name) + 28) for name in agents}
    if "positions" in data:
        return [Node(name, *data["positions"][name], widths[name]) for name in agents]
    if not agents:
        return []
    ids = {name: f"a{i}" for i, name in enumerate(agents)}
    gids = {name: f"g{i}" for i, name in enumerate(groups) if groups[name]}
    adjacency = {node: [] for node in [*ids.values(), *gids.values()]}
    edges = []
    for group, gid in gids.items():
        for member in groups[group]:
            aid = ids[member]
            edges.append((gid, aid))
            adjacency[gid].append(aid)
            adjacency[aid].append(gid)
    graph = ['graph {', 'graph [nodesep=0.9, ranksep=0.9];',
             'node [shape=box, fixedsize=true, label=""];']
    for name, aid in ids.items():
        graph.append(f'{aid} [width={widths[name] / 72:.4f}, height={64 / 72:.4f}];')
    for gid in gids.values():
        graph.append(f'{gid} [width=0.1, height=0.1];')
    # Prefer the group that shares the most members with other groups. Resolve
    # disconnected components independently, including agents without a group.
    roots = sorted(gids.values(), key=lambda gid: (
        -sum(len(adjacency[aid]) - 1 for aid in adjacency[gid]), -len(adjacency[gid]), gid))
    if root in gids:
        roots.remove(gids[root])
        roots.insert(0, gids[root])
    levels, component_roots = {}, []
    for origin in roots + list(ids.values()):
        if origin in levels:
            continue
        component_roots.append(origin)
        levels[origin] = 0 if origin in gids.values() else 2
        queue = [origin]
        for node in queue:
            for neighbor in adjacency[node]:
                if neighbor not in levels:
                    levels[neighbor] = levels[node] + 2
                    queue.append(neighbor)
    ranks = [[n for n in adjacency if levels[n] == level] for level in sorted(set(levels.values()))]
    for rank in ranks:
        graph.append('{rank=same; ' + '; '.join(rank) + ';}')
    edges = [(a, b) if levels[a] <= levels[b] else (b, a) for a, b in edges]
    graph.extend(f'{a} -- {b};' for a, b in edges)
    graph.append('}')
    engine = "dot"
    try:
        result = subprocess.run([engine, "-Tplain"], input="\n".join(graph), text=True,
                                capture_output=True, check=True, timeout=60)
    except FileNotFoundError as error:
        raise ValueError(f"Graphviz {engine} is required for automatic layout") from error
    except subprocess.CalledProcessError as error:
        raise ValueError(f"Graphviz failed: {error.stderr.strip()}") from error
    except subprocess.TimeoutExpired as error:
        raise ValueError("Graphviz layout timed out after 60 seconds") from error
    coords = {}
    for row in result.stdout.splitlines():
        fields = row.split()
        if fields[0] == "node":
            coords[fields[1]] = (float(fields[2]) * 72, -float(fields[3]) * 72)
    # Lift a root group's exclusive members above the members linking its teams.
    # Keep the horizontal order and room Graphviz reserved for every card.
    for origin in component_roots:
        if origin not in gids.values():
            continue
        leaves = [n for n in adjacency[origin] if len(adjacency[n]) == 1]
        shared = [n for n in adjacency[origin] if len(adjacency[n]) > 1]
        if shared:
            for node in leaves:
                coords[node] = (coords[node][0], coords[node][1] - 130)
    return [Node(name, *coords[ids[name]], widths[name]) for name in agents]


def route(start, end, obstacles):
    """Shortest visibility-graph path around padded nonmember rectangles."""
    for padding in (32, 20, 10):
        rectangles = [node.shape(padding) for node in obstacles]
        interior = prep(unary_union([rect.buffer(-0.01) for rect in rectangles]))
        direct = LineString([start, end])
        if not interior.intersects(direct):
            return [start, end]
        vertices = [start, end]
        for rect in rectangles:
            vertices.extend(list(rect.exterior.coords)[:-1])
        neighbors = [[] for _ in vertices]
        for i, a in enumerate(vertices):
            for j in range(i):
                b = vertices[j]
                if not interior.intersects(LineString([a, b])):
                    distance = math.dist(a, b)
                    neighbors[i].append((j, distance))
                    neighbors[j].append((i, distance))
        distances, parents, queue = {0: 0}, {}, [(0, 0)]
        while queue:
            distance, index = heapq.heappop(queue)
            if distance != distances[index]:
                continue
            if index == 1:
                indices = [1]
                while indices[-1] != 0:
                    indices.append(parents[indices[-1]])
                return [vertices[i] for i in reversed(indices)]
            for neighbor, cost in neighbors[index]:
                candidate = distance + cost
                if candidate < distances.get(neighbor, math.inf):
                    distances[neighbor], parents[neighbor] = candidate, index
                    heapq.heappush(queue, (candidate, neighbor))
    raise ValueError("cannot route between members; increase agent spacing")


def virtual_edges(members, others):
    """Prim's minimum spanning tree, with each edge rerouted around nonmembers."""
    visited, remaining, paths = [members[0]], list(members[1:]), []
    while remaining:
        a, b = min(((a, b) for a in visited for b in remaining),
                   key=lambda pair: math.dist(pair[0].center, pair[1].center))
        paths.append(route(a.center, b.center, others))
        visited.append(b)
        remaining.remove(b)
    return paths


def node_distance(x, y, node):
    return np.hypot(np.maximum(np.abs(x - node.x) - node.width / 2, 0),
                    np.maximum(np.abs(y - node.y) - node.height / 2, 0))


def segment_distance(x, y, a, b):
    dx, dy = b[0] - a[0], b[1] - a[1]
    length2 = dx * dx + dy * dy
    if not length2:
        return np.hypot(x - a[0], y - a[1])
    t = np.clip(((x - a[0]) * dx + (y - a[1]) * dy) / length2, 0, 1)
    return np.hypot(x - a[0] - t * dx, y - a[1] - t * dy)


def influence(distance, inner=18, outer=65):
    # At distance inner the contribution is 1; it falls quadratically to zero.
    return (np.maximum(outer - distance, 0) / (outer - inner)) ** 2


def smooth_ring(coords):
    """Closed quadratic B-spline, plus a fine polygonal approximation for validation."""
    points = list(Polygon(coords).simplify(0.7, preserve_topology=True).exterior.coords)[:-1]
    mids = [((a[0] + b[0]) / 2, (a[1] + b[1]) / 2)
            for a, b in zip(points[-1:] + points[:-1], points)]
    commands = [f"M {mids[0][0]:.3f},{mids[0][1]:.3f}"]
    sampled = []
    for i, control in enumerate(points):
        start, end = mids[i], mids[(i + 1) % len(points)]
        commands.append(f"Q {control[0]:.3f},{control[1]:.3f} {end[0]:.3f},{end[1]:.3f}")
        for t in np.linspace(0, 1, 9, endpoint=False):
            sampled.append(tuple((1 - t) ** 2 * start[k] + 2 * (1 - t) * t * control[k]
                                 + t * t * end[k] for k in (0, 1)))
    return " ".join(commands) + " Z", sampled


def bubble(name, members, others, step=3):
    """Positive member/edge fields + negative obstacles, then marching squares.

    Based on the Bubble Sets method (Collins, Penn, Carpendale, 2009):
    https://vialab.ca/research/bubble-sets/
    This variant uses visibility routing and checks the smoothed result, including
    holes, rather than relaxing membership correctness to force a contour.
    """
    paths = virtual_edges(members, others)
    bounds = [node.bounds() for node in members]
    xs = [v for bound in bounds for v in (bound[0], bound[2])]
    ys = [v for bound in bounds for v in (bound[1], bound[3])]
    xs.extend(p[0] for path in paths for p in path)
    ys.extend(p[1] for path in paths for p in path)
    columns = (max(xs) - min(xs) + 200) / step + 2
    rows = (max(ys) - min(ys) + 200) / step + 2
    if columns * rows > 4_000_000:
        raise ValueError(f"group {name!r} needs too large a grid; increase --grid-step")
    x = np.arange(min(xs) - 100, max(xs) + 100 + step, step)
    y = np.arange(min(ys) - 100, max(ys) + 100 + step, step)
    xx, yy = np.meshgrid(x, y)
    positive = sum(influence(node_distance(xx, yy, node)) for node in members)
    for path in paths:
        # Use the nearest segment of each routed edge to avoid bulges at bends.
        distance = np.minimum.reduce([segment_distance(xx, yy, a, b)
                                      for a, b in itertools.pairwise(path)])
        positive += influence(distance, inner=22)
    negative = np.zeros_like(positive)
    for node in others:
        negative += influence(node_distance(xx, yy, node))
    required = [node.shape(2) for node in members]
    excluded = [node.shape(2) for node in others]
    for weight in (1.0, 2.0, 4.0, 8.0):
        field = positive - weight * negative
        contours = contourpy.contour_generator(x=x, y=y, z=field, fill_type="OuterOffset")
        for threshold in (1.0, 0.8, 0.6, 0.4):
            points, offsets = contours.filled(threshold, float(positive.max()) + 1)
            for coordinates, boundaries in zip(points, offsets):
                rings = [coordinates[a:b] for a, b in itertools.pairwise(boundaries)]
                raw = Polygon(rings[0], rings[1:])
                if not all(raw.covers(shape) for shape in required):
                    continue
                curves = [smooth_ring(ring) for ring in rings]
                shape = Polygon(curves[0][1], [curve[1] for curve in curves[1:]])
                if (shape.is_valid and all(shape.covers(rect) for rect in required)
                        and not any(shape.intersects(rect) for rect in excluded)):
                    return Bubble(name, " ".join(curve[0] for curve in curves), shape)
    raise ValueError(f"no faithful contour for group {name!r}; increase agent spacing or reduce --grid-step")


def build(data, root=None, step=3):
    nodes = layout(data, root)
    for i, node in enumerate(nodes):
        if any(node.shape(4).intersects(other.shape(4)) for other in nodes[:i]):
            raise ValueError("agent cards overlap; increase spacing in positions")
    if nodes:
        left = min(node.bounds()[0] for node in nodes)
        top = min(node.bounds()[1] for node in nodes)
        nodes = [Node(n.name, n.x - left + 160, n.y - top + 185, n.width) for n in nodes]
    bubbles = []
    for name, members in data["groups"].items():
        if members:
            member_names = set(members)
            bubbles.append(bubble(name, [n for n in nodes if n.name in member_names],
                                  [n for n in nodes if n.name not in member_names], step))
    return nodes, bubbles


PALETTE = ("#8562cc", "#278f80", "#cb862f", "#447fc3", "#bc577f", "#718b35")


def render(data, nodes, bubbles):
    """Render geometry and a membership legend; identical sets use alternating dashes."""
    groups = data["groups"]
    bounds = [n.bounds() for n in nodes] + [b.shape.bounds for b in bubbles]
    left = min([0] + [b[0] - 40 for b in bounds])
    top = min([0] + [b[1] - 130 for b in bounds])
    right = max([900] + [b[2] + 80 for b in bounds])
    bottom = max([220] + [b[3] + 70 for b in bounds])
    legend_y = bottom
    # One legend row per group avoids truncating arbitrary names or memberships.
    longest = max([0] + [len(name) + len(", ".join(members)) + 4 for name, members in groups.items()])
    right = max(right, left + longest * 8 + 140)
    bottom += 46 * len(groups) + 70
    width, height = math.ceil(right - left), math.ceil(bottom - top)
    output = [(f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" '
              f'viewBox="{left} {top} {width} {height}" role="img" aria-labelledby="title description">'),
              '<title id="title">BBS agent circles</title>',
              ('<desc id="description">Each contour contains exactly its group members. '
              'Shared agents appear in overlapping contours. Empty groups appear only in the legend.</desc>'),
              f'<metadata>{escape(json.dumps(data, ensure_ascii=False))}</metadata>',
              ('<style>text {font-family:DejaVu Sans,sans-serif} '
              '.name {font-size:17px;font-weight:bold;fill:#202b40;text-anchor:middle} '
              '.detail {font-size:12px;fill:#6b7788;text-anchor:middle}</style>'),
              f'<rect x="{left}" y="{top}" width="{width}" height="{height}" fill="#fafbfe"/>',
              f'<text x="{left + 60}" y="{top + 65}" font-size="30" font-weight="bold" fill="#202b40">Agent circles</text>',
              (f'<text x="{left + 60}" y="{top + 96}" font-size="15" fill="#697386">'
              f'{len(nodes)} agents · {len(groups)} groups · Bubble Sets</text>')]
    colors = {}
    for i, name in enumerate(groups):
        rgb = colorsys.hsv_to_rgb((i * 0.61803398875) % 1, 0.58, 0.65)
        colors[name] = PALETTE[i] if i < len(PALETTE) else "#" + "".join(f"{round(c * 255):02x}" for c in rgb)
    # Alternate colors along coincident contours so no group's stroke is hidden.
    duplicates = {}
    for name, members in groups.items():
        duplicates.setdefault(tuple(members), []).append(name)
    for item in bubbles:
        twins = duplicates[tuple(groups[item.name])]
        dash = (f' stroke-dasharray="12 {12 * (len(twins) - 1)}" '
                f'stroke-dashoffset="{-12 * twins.index(item.name)}"') if len(twins) > 1 else ""
        output.append(f'<path d="{item.path}" fill="{colors[item.name]}" fill-opacity="0.11" '
                      f'fill-rule="evenodd" stroke="{colors[item.name]}" stroke-width="2.5"{dash}>'
                      f'<title>{escape(item.name)}</title></path>')
    for node in nodes:
        membership = [g for g, members in groups.items() if node.name in members]
        x, y, _, _ = node.bounds()
        output.extend([f'<g><title>{escape(node.name + ": " + (", ".join(membership) or "ungrouped"))}</title>',
                       (f'<rect x="{x:.3f}" y="{y:.3f}" width="{node.width}" height="{node.height}" '
                       'rx="22" fill="white" stroke="#dce1e9" stroke-width="1.5"/>'),
                       f'<text x="{node.x:.3f}" y="{node.y - 2:.3f}" class="name">{escape(node.name)}</text>',
                       (f'<text x="{node.x:.3f}" y="{node.y + 18:.3f}" class="detail">'
                       f'{len(membership)} group{"s" if len(membership) != 1 else ""}</text></g>')])
    for i, (name, members) in enumerate(groups.items()):
        y = legend_y + 46 * i
        output.extend([f'<rect x="{left + 62}" y="{y - 13}" width="15" height="15" rx="5" fill="{colors[name]}"/>',
                       (f'<text x="{left + 94}" y="{y}" font-size="14" fill="#354158">'
                       f'<tspan font-weight="bold">{escape(name)}</tspan> · '
                       f'{escape(", ".join(members) or "no agents")}</text>')])
    output.append(f'<text x="{left + 60}" y="{bottom - 30}" font-size="12" fill="#697386">'
                  'Each agent appears once. Overlapping contours indicate shared membership.</text></svg>')
    return "\n".join(output) + "\n", width, height


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--input", type=Path, help="JSON snapshot; '-' reads stdin (default: live BBS)")
    parser.add_argument("--bbs", default="bbs", help="BBS executable (default: bbs)")
    parser.add_argument("--root-group", help="hierarchy root (default: the group with most shared memberships)")
    parser.add_argument("--output", type=Path, default=Path("agents.svg"), help="SVG destination")
    parser.add_argument("--png", type=Path, help="also render a PNG")
    parser.add_argument("--longest-edge", type=int, default=4096, help="PNG longest edge in pixels (default: 4096)")
    parser.add_argument("--snapshot", type=Path, help="save the input memberships as JSON")
    parser.add_argument("--grid-step", type=float, default=3, help="potential-field grid spacing (default: 3)")
    args = parser.parse_args()
    try:
        if not math.isfinite(args.grid_step) or args.grid_step < 0.5 or args.grid_step > 10:
            raise ValueError("--grid-step must be between 0.5 and 10")
        if not 1 <= args.longest_edge <= 16384:
            raise ValueError("--longest-edge must be between 1 and 16384")
        outputs = [p.resolve() for p in (args.output, args.png, args.snapshot) if p]
        if len(outputs) != len(set(outputs)):
            raise ValueError("output, PNG, and snapshot destinations must be different")
        if args.input and str(args.input) != "-" and args.input.resolve() in outputs:
            raise ValueError("output destinations must not overwrite the input")
        if args.input:
            data = normalize(json.loads(sys.stdin.read() if str(args.input) == "-" else args.input.read_text()))
        else:
            data = read_bbs(args.bbs)
        nodes, bubbles = build(data, args.root_group, args.grid_step)
        svg, width, height = render(data, nodes, bubbles)
        png = None
        if args.png:
            import cairosvg
            scale = args.longest_edge / max(width, height)
            png = cairosvg.svg2png(bytestring=svg.encode(), output_width=max(1, round(width * scale)),
                                  output_height=max(1, round(height * scale)))
        for destination, content in ((args.output, svg.encode()), (args.png, png),
                                     (args.snapshot, (json.dumps(data, indent=2) + "\n").encode())):
            if destination:
                destination.parent.mkdir(parents=True, exist_ok=True)
                destination.write_bytes(content)
                print(destination)
        print(f"Validated {len(bubbles)} contours against {len(nodes)} agent cards.", file=sys.stderr)
    except (TypeError, ValueError, OSError) as error:
        parser.exit(1, f"error: {error}\n")


if __name__ == "__main__":
    main()
