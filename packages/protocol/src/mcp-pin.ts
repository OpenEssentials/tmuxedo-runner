/**
 * MCP package pinning (PLAN Phase 17 box 5; docs/protocol.md "Plugin
 * settings", "Pinned packages").
 *
 * A local MCP server is often a package fetched at launch: `npx pkg`,
 * `uvx pkg`, `docker run image`. Without an exact version, whatever is
 * published next runs on the person's machine with their secrets in its
 * environment. `mcpPackage` reads the package and its version out of the
 * argv; a launch is PINNED when it names one exact version (npm: `1.2.3`;
 * PyPI: `==1.2.3` or `@1.2.3`; images: `@sha256:<64 hex>`). Ranges, tags
 * (`latest`, `next`, an image tag) and no version at all are not pinned.
 *
 * Mirrored in Rust (runner/crates/runner/src/mcp_pin.rs, `package_pin`)
 * on fixtures/mcp-pin.json. A command that is none of these launchers, or
 * that runs a local path, is not a package launch (null).
 */

export type PackageRegistry = 'npm' | 'pypi' | 'oci';
export type PinProblem = 'missing' | 'range' | 'tag' | 'not_registry' | 'bad_name';
export interface PackageLaunch {
  registry: PackageRegistry;
  /** `npx`, `uvx`, `docker run`, … (the program's base name, and its subcommand). */
  launcher: string;
  /** The argv element holding the package. */
  index: number;
  /** What precedes the name in that element (`--package=`, `--from=`, or ''). */
  prefix: string;
  /** The package (npm, PyPI) or image (without tag or digest). */
  name: string;
  /** PyPI extras as written (`[cli]`), kept when the version is rewritten. */
  extras: string;
  /** The version part as written, without its separator ('' when none). */
  spec: string;
  /** How the version is joined to the name: `@`, `==` or (an image tag) `:`. */
  sep: '@' | '==' | ':' | '';
  /** The exact version or digest when pinned. */
  version: string | null;
  pinned: boolean;
  problem: PinProblem | null;
  /** Another index than the public one is named, so nothing is looked up for it. */
  customIndex: boolean;
}

/** npm package names (lower case, optional scope), at most 214 characters. */
export const NPM_PACKAGE_NAME = /^(?:@[a-z0-9][a-z0-9._~-]*\/)?[a-z0-9][a-z0-9._~-]*$/;
/** PyPI project names (PEP 508). */
export const PYPI_PACKAGE_NAME = /^[A-Za-z0-9](?:[A-Za-z0-9._-]{0,98}[A-Za-z0-9])?$/;
const OCI_NAME = /^[a-z0-9][a-z0-9._-]*(?::[0-9]{1,5})?(?:\/[a-z0-9][a-z0-9._-]*)*$/;
const OCI_TAG = /^[A-Za-z0-9_][A-Za-z0-9._-]{0,127}$/;
/** An exact semantic version: npm resolves it to exactly one release. */
export const NPM_EXACT = /^(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)(?:-[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$/;
/** An exact PEP 440 version (no wildcard). */
export const PYPI_EXACT = /^(?:\d+!)?\d+(?:\.\d+)*(?:(?:a|b|rc)\d+)?(?:\.post\d+)?(?:\.dev\d+)?(?:\+[a-z0-9]+(?:\.[a-z0-9]+)*)?$/i;
export const OCI_DIGEST = /^sha256:[a-f0-9]{64}$/;
const TAG_WORD = /^[A-Za-z][A-Za-z0-9._-]*$/;

/** Options that take a value, per launcher, so the value is not read as the package. */
const VALUE_OPTS: Record<string, readonly string[]> = {
  npx: ['-p', '--package', '-c', '--call', '--registry', '--cache', '--userconfig', '-w', '--workspace'],
  uvx: ['--from', '--with', '-w', '--with-requirements', '--with-editable', '--python', '-p', '--index', '--index-url', '-i', '--extra-index-url', '--default-index',
    '--find-links', '-f', '--constraints', '-c', '--overrides', '--cache-dir', '--directory', '--project', '--config-file', '--env-file', '--prerelease',
    '--resolution', '--exclude-newer', '--refresh-package', '--upgrade-package', '-P', '--reinstall-package', '--color', '--python-preference', '--index-strategy',
    '--keyring-provider', '--link-mode'],
  pipx: ['--spec', '--python', '--pip-args', '--index-url', '-i', '--backend'],
  docker: ['-a', '--attach', '--add-host', '--cap-add', '--cap-drop', '--cidfile', '--cpus', '--cpu-shares', '-c', '--cpuset-cpus', '--device', '--dns', '-e', '--env',
    '--env-file', '--entrypoint', '--expose', '--group-add', '-h', '--hostname', '--ip', '-l', '--label', '--label-file', '--log-driver', '--log-opt', '-m', '--memory',
    '--mount', '--name', '--network', '--net', '--network-alias', '-p', '--publish', '--platform', '--pull', '--restart', '--runtime', '--security-opt', '--shm-size',
    '--stop-signal', '--stop-timeout', '--tmpfs', '--ulimit', '-u', '--user', '--userns', '-v', '--volume', '--volumes-from', '-w', '--workdir', '--health-cmd', '--ipc',
    '--pid', '--uts', '--gpus', '--isolation', '--sysctl'],
};
/** The option that names the package outright (`npx -p pkg cmd`, `uvx --from pkg cmd`, `pipx run --spec pkg app`). */
const PACKAGE_OPTS: Record<string, readonly string[]> = { npx: ['-p', '--package'], uvx: ['--from'], pipx: ['--spec'], docker: [] };
const CUSTOM_INDEX = ['--registry', '--index', '--index-url', '-i', '--extra-index-url', '--default-index', '--find-links', '-f'];

const baseName = (p: string) => p.slice(p.lastIndexOf('/') + 1).replace(/\.(?:cmd|exe)$/i, '').toLowerCase();

interface Launcher { launcher: string; registry: PackageRegistry; start: number; opts: string }
function launcherOf(argv: readonly string[]): Launcher | null {
  const p = baseName(argv[0] ?? '');
  const sub = argv[1];
  if (p === 'npx' || p === 'bunx') return { launcher: p, registry: 'npm', start: 1, opts: 'npx' };
  if ((p === 'pnpm' || p === 'yarn') && sub === 'dlx') return { launcher: `${p} dlx`, registry: 'npm', start: 2, opts: 'npx' };
  if (p === 'npm' && (sub === 'exec' || sub === 'x')) return { launcher: `npm ${sub}`, registry: 'npm', start: 2, opts: 'npx' };
  if (p === 'uvx') return { launcher: 'uvx', registry: 'pypi', start: 1, opts: 'uvx' };
  if (p === 'uv' && sub === 'tool' && argv[2] === 'run') return { launcher: 'uv tool run', registry: 'pypi', start: 3, opts: 'uvx' };
  if (p === 'pipx' && sub === 'run') return { launcher: 'pipx run', registry: 'pypi', start: 2, opts: 'pipx' };
  if ((p === 'docker' || p === 'podman') && sub === 'run') return { launcher: `${p} run`, registry: 'oci', start: 2, opts: 'docker' };
  if ((p === 'docker' || p === 'podman') && sub === 'container' && argv[2] === 'run') return { launcher: `${p} run`, registry: 'oci', start: 3, opts: 'docker' };
  return null;
}

const isLocal = (s: string) => /^(?:\.{1,2}(?:\/|$)|\/|~)/.test(s);
const isAddress = (s: string) => /^[a-z][a-z0-9+.-]*:/i.test(s);

function npmSpec(l: PackageLaunch, s: string): PackageLaunch | null {
  if (isLocal(s)) return null;
  // An address, a repository spec or the user/repo shorthand: not a registry version.
  if (isAddress(s) || /^[^@/][^/]*\/./.test(s)) return { ...l, name: s, problem: 'not_registry' };
  const at = s.indexOf('@', 1);
  const name = at < 0 ? s : s.slice(0, at);
  const spec = at < 0 ? '' : s.slice(at + 1);
  const r: PackageLaunch = { ...l, name, spec, sep: at < 0 ? '' : '@' };
  if (!name || name.length > 214 || !NPM_PACKAGE_NAME.test(name)) return { ...r, problem: 'bad_name' };
  if (at < 0) return { ...r, problem: 'missing' };
  if (NPM_EXACT.test(spec)) return { ...r, version: spec, pinned: true, problem: null };
  return { ...r, problem: TAG_WORD.test(spec) ? 'tag' : 'range' };
}

function pypiSpec(l: PackageLaunch, s: string): PackageLaunch | null {
  if (isLocal(s)) return null;
  if (isAddress(s) || / @ /.test(s)) return { ...l, name: s, problem: 'not_registry' };
  const m = /^([^[=<>!~@;\s]*)(\[[A-Za-z0-9._,\s-]*\])?\s*(.*)$/s.exec(s)!;
  const name = m[1]!;
  const extras = m[2] ?? '';
  const rest = m[3]!.trim();
  let sep: PackageLaunch['sep'] = '';
  let spec = rest;
  if (rest.startsWith('==') && !rest.startsWith('===')) { sep = '=='; spec = rest.slice(2).trim(); } else if (rest.startsWith('@')) { sep = '@'; spec = rest.slice(1).trim(); }
  const r: PackageLaunch = { ...l, name, extras, spec, sep };
  if (!PYPI_PACKAGE_NAME.test(name)) return { ...r, problem: 'bad_name' };
  if (!rest) return { ...r, problem: 'missing' };
  if (sep && PYPI_EXACT.test(spec)) return { ...r, version: spec, pinned: true, problem: null };
  return { ...r, problem: sep === '@' && TAG_WORD.test(spec) ? 'tag' : 'range' };
}

function ociSpec(l: PackageLaunch, s: string): PackageLaunch {
  const at = s.indexOf('@');
  const ref = at < 0 ? s : s.slice(0, at);
  const digest = at < 0 ? '' : s.slice(at + 1);
  // A tag follows the last ':' after the last '/'; an earlier ':' is a registry's port.
  const colon = ref.lastIndexOf(':');
  const hasTag = colon > ref.lastIndexOf('/');
  const name = hasTag ? ref.slice(0, colon) : ref;
  const tag = hasTag ? ref.slice(colon + 1) : '';
  const r: PackageLaunch = { ...l, name, spec: digest || tag, sep: digest ? '@' : hasTag ? ':' : '' };
  if (!name || name.length > 255 || !OCI_NAME.test(name) || (hasTag && !OCI_TAG.test(tag))) return { ...r, problem: 'bad_name' };
  if (at >= 0) return OCI_DIGEST.test(digest) ? { ...r, version: digest, pinned: true, problem: null } : { ...r, problem: 'bad_name' };
  return { ...r, problem: hasTag ? 'tag' : 'missing' };
}

/** The package an MCP server's command fetches, or null when it is not a package launch. */
export function mcpPackage(command: readonly unknown[]): PackageLaunch | null {
  if (!Array.isArray(command) || !command.every((a): a is string => typeof a === 'string')) return null;
  const argv = command as readonly string[];
  const l = launcherOf(argv);
  if (!l) return null;
  const valued = VALUE_OPTS[l.opts]!;
  const packageOpt = PACKAGE_OPTS[l.opts]!;
  let customIndex = false;
  let found: { index: number; prefix: string; value: string } | null = null;
  for (let i = l.start; i < argv.length; i++) {
    const a = argv[i]!;
    if (a === '--') {
      if (!found && i + 1 < argv.length) found = { index: i + 1, prefix: '', value: argv[i + 1]! };
      break;
    }
    if (a.startsWith('-') && a.length > 1) {
      const eq = a.indexOf('=');
      const opt = eq < 0 ? a : a.slice(0, eq);
      if (CUSTOM_INDEX.includes(opt)) customIndex = true;
      if (!found && packageOpt.includes(opt)) {
        if (eq >= 0) found = { index: i, prefix: a.slice(0, eq + 1), value: a.slice(eq + 1) };
        else if (i + 1 < argv.length) found = { index: i + 1, prefix: '', value: argv[i + 1]! };
      }
      if (eq < 0 && valued.includes(opt)) i++;
      continue;
    }
    if (!found) found = { index: i, prefix: '', value: a };
    break;
  }
  if (!found) return null;
  const empty: PackageLaunch = { registry: l.registry, launcher: l.launcher, index: found.index, prefix: found.prefix, name: '', extras: '', spec: '', sep: '', version: null, pinned: false, problem: 'missing', customIndex };
  if (l.registry === 'npm') return npmSpec(empty, found.value);
  if (l.registry === 'pypi') return pypiSpec(empty, found.value);
  return ociSpec(empty, found.value);
}

/** The command with the package pinned to `version`, or null when that is not an exact version (or digest, for an image). */
export function pinCommand(command: readonly string[], launch: PackageLaunch, version: string): string[] | null {
  if (launch.problem === 'bad_name' || launch.problem === 'not_registry') return null;
  const out = [...command];
  if (launch.registry === 'oci') {
    if (!OCI_DIGEST.test(version)) return null;
    out[launch.index] = `${launch.prefix}${launch.name}${launch.sep === ':' ? `:${launch.spec}` : ''}@${version}`;
    return out;
  }
  if (!(launch.registry === 'npm' ? NPM_EXACT : PYPI_EXACT).test(version)) return null;
  const sep = launch.registry === 'npm' || launch.sep === '@' ? '@' : '==';
  out[launch.index] = `${launch.prefix}${launch.name}${launch.extras}${sep}${version}`;
  return out;
}

/** How a pinned package is written, for the sentence that asks for one. */
export function pinExample(launch: Pick<PackageLaunch, 'registry' | 'name'>): string {
  return launch.registry === 'npm' ? `${launch.name}@1.2.3` : launch.registry === 'pypi' ? `${launch.name}==1.2.3` : `${launch.name}@sha256:…`;
}

/** One sentence for a launch that is not pinned (null when it is). */
export function pinProblemText(launch: PackageLaunch): string | null {
  if (launch.pinned) return null;
  const name = launch.name.slice(0, 80);
  switch (launch.problem) {
    case 'bad_name': return `"${name}" is not a valid ${launch.registry === 'oci' ? 'image' : 'package'} name, or its version is not written the usual way.`;
    case 'not_registry': return 'This fetches code from an address, which can change at any time. Use a registry package with an exact version, or a program installed on the machine.';
    case 'missing': return `Pin ${name} to an exact version, like ${pinExample(launch)}: without one, whatever is published next would run.`;
    default: return `"${launch.spec.slice(0, 40)}" can change under you. Pin ${name} to an exact version, like ${pinExample(launch)}.`;
  }
}

/** A stdio definition's package launch that may not be saved as new: the sentence, or null. */
export function unpinnedProblem(definition: unknown): string | null {
  if (!definition || typeof definition !== 'object') return null;
  const d = definition as { transport?: unknown; command?: unknown };
  if (d.transport !== 'stdio' || !Array.isArray(d.command)) return null;
  const l = mcpPackage(d.command);
  return l ? pinProblemText(l) : null;
}
