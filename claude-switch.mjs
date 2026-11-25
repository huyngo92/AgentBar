#!/usr/bin/env node
// claude-switch.mjs — Zero-dependency Claude gateway profile switcher
import { readFileSync, writeFileSync, existsSync, mkdirSync, unlinkSync, chmodSync } from 'fs'
import { join, dirname } from 'path'
import { fileURLToPath } from 'url'
import { homedir } from 'os'

const __dirname = dirname(fileURLToPath(import.meta.url))
const PROFILES_PATH = join(__dirname, 'profiles.json')
const ANTIGRAVITY_PATH = join(__dirname, 'antigravity.json')
const BACKUP_FORMAT = 'claude-gemini-gateway-backup'
const BACKUP_VERSION = 2
const SETTINGS_PATH = join(homedir(), '.claude', 'settings.json')
// Snapshot of the native login's ANTHROPIC_* env (shared with the Tauri app)
const NATIVE_BACKUP_PATH = join(homedir(), '.claude', 'gateway-switcher-native-backup.json')
const CLAUDE_JSON_PATH = join(homedir(), '.claude.json')
// Failover mode is run by the menubar app (local proxy across Claude profiles).
const FAILOVER_KEY = '__failover__'
const isFailover = (data, env = {}) => data.active === FAILOVER_KEY && /^http:\/\/127\.0\.0\.1:\d+$/.test(env.ANTHROPIC_BASE_URL || '')
const NATIVE_KEY = '__native__'
const CRED_KEYS = ['ANTHROPIC_BASE_URL', 'ANTHROPIC_AUTH_TOKEN', 'ANTHROPIC_API_KEY']

function loadProfiles() {
  return JSON.parse(readFileSync(PROFILES_PATH, 'utf8'))
}

function saveProfiles(data) {
  writeFileSync(PROFILES_PATH, JSON.stringify(data, null, 2) + '\n')
}

function loadSettings() {
  if (!existsSync(SETTINGS_PATH)) return { hasCompletedOnboarding: true, env: {} }
  return JSON.parse(readFileSync(SETTINGS_PATH, 'utf8'))
}

function saveSettings(settings) {
  mkdirSync(dirname(SETTINGS_PATH), { recursive: true })
  writeFileSync(SETTINGS_PATH, JSON.stringify(settings, null, 2) + '\n')
}

// Keys the switcher owns in settings.env: ANTHROPIC_*, Claude Code model vars, and
// anything any profile sets. Switching replaces exactly these (same rule as the app).
const EXTRA_MANAGED = ['CLAUDE_CODE_SUBAGENT_MODEL']
function managedKeys(data) {
  const keys = new Set(EXTRA_MANAGED)
  for (const p of Object.values(data.profiles)) for (const k of Object.keys(p.env || {})) keys.add(k)
  return keys
}
const isManaged = (k, extra) => k.startsWith('ANTHROPIC_') || extra.has(k)
const pickEnv = (env = {}, keep) => Object.fromEntries(Object.entries(env).filter(([k]) => keep(k)))

// claude-switch models: slot name -> env var
const MODEL_SLOTS = {
  opus: 'ANTHROPIC_DEFAULT_OPUS_MODEL',
  sonnet: 'ANTHROPIC_DEFAULT_SONNET_MODEL',
  haiku: 'ANTHROPIC_DEFAULT_HAIKU_MODEL',
  main: 'ANTHROPIC_MODEL',
  subagent: 'CLAUDE_CODE_SUBAGENT_MODEL'
}
const LATEST_CLAUDE = { opus: 'claude-opus-5-5', sonnet: 'claude-sonnet-5-5', haiku: 'claude-haiku-5-5' }

// Native login is active unless the `active` profile's credentials are what settings.json holds.
function isNativeActive(data, env = {}) {
  if (isFailover(data, env)) return false
  const p = data.profiles[data.active]
  return !p || CRED_KEYS.some(k => p.env[k] !== env[k])
}

function nativeInfo() {
  let acc = null
  try { acc = JSON.parse(readFileSync(CLAUDE_JSON_PATH, 'utf8')).oauthAccount || null } catch {}
  return {
    loggedIn: !!acc,
    email: acc?.emailAddress || '',
    organization: acc?.organizationName || '',
    billingType: acc?.billingType || '',
    active: isNativeActive(loadProfiles(), loadSettings().env),
    hasBackup: existsSync(NATIVE_BACKUP_PATH)
  }
}

function restoreNative() {
  const settings = loadSettings()
  const managed = managedKeys(loadProfiles())
  const env = pickEnv(settings.env, k => !isManaged(k, managed))
  if (existsSync(NATIVE_BACKUP_PATH)) {
    Object.assign(env, JSON.parse(readFileSync(NATIVE_BACKUP_PATH, 'utf8')).env || {})
  }
  settings.env = env
  saveSettings(settings)
  if (existsSync(NATIVE_BACKUP_PATH)) unlinkSync(NATIVE_BACKUP_PATH)
  const data = loadProfiles()
  if (data.active) { data.active = ''; saveProfiles(data) }
  const info = nativeInfo()
  console.log(`✓ Restored Claude login${info.email ? `: ${info.email}` : info.loggedIn ? '' : ' (not logged in — run `claude /login`)'}`)
}

function applyProfile(profileKey) {
  if (profileKey === NATIVE_KEY || profileKey === 'native') return restoreNative()
  const data = loadProfiles()
  const profile = data.profiles[profileKey]
  if (!profile) {
    console.error(`Profile "${profileKey}" not found. Available: native, ${Object.keys(data.profiles).join(', ')}`)
    process.exit(1)
  }

  const settings = loadSettings()
  const managed = managedKeys(data)
  // Leaving the native login: snapshot its managed env once so `restore` can bring it back.
  if (!existsSync(NATIVE_BACKUP_PATH)) {
    const env = isNativeActive(data, settings.env) ? pickEnv(settings.env, k => isManaged(k, managed)) : {}
    mkdirSync(dirname(NATIVE_BACKUP_PATH), { recursive: true })
    writeFileSync(NATIVE_BACKUP_PATH, JSON.stringify({ env, savedAt: Math.floor(Date.now() / 1000) }, null, 2) + '\n', { mode: 0o600 })
    chmodSync(NATIVE_BACKUP_PATH, 0o600)
  }
  // Keep the user's own vars, replace everything the switcher manages
  settings.env = { ...pickEnv(settings.env, k => !isManaged(k, managed)), ...profile.env }
  saveSettings(settings)

  data.active = profileKey
  saveProfiles(data)

  console.log(`✓ Switched to: ${profile.name} (${profileKey})`)
  console.log(`  Gateway: ${profile.env.ANTHROPIC_BASE_URL || 'default'}`)
}

function listProfiles() {
  const data = loadProfiles()
  const native = nativeInfo()
  console.log('Claude Gateway Profiles:\n')
  console.log(`  ${native.active ? '→' : ' '} ${'native'.padEnd(15)} Claude Login`)
  console.log(`    ${' '.repeat(15)} ${native.loggedIn ? [native.email, native.organization].filter(Boolean).join(' · ') : 'not logged in'}`)
  if (isFailover(data, loadSettings().env)) {
    console.log(`  → ${'failover'.padEnd(15)} Failover (proxy trong app: ${loadSettings().env.ANTHROPIC_BASE_URL})`)
  }
  for (const [key, profile] of Object.entries(data.profiles)) {
    const marker = !native.active && key === data.active ? '→' : ' '
    console.log(`  ${marker} ${key.padEnd(15)} ${profile.name}`)
    console.log(`    ${' '.repeat(15)} ${profile.env.ANTHROPIC_BASE_URL || 'default'}`)
  }
}

function showCurrent() {
  const data = loadProfiles()
  const native = nativeInfo()
  if (native.active) {
    console.log(JSON.stringify({ profile: 'native', name: 'Claude Login', ...native }, null, 2))
    return
  }
  if (isFailover(data, loadSettings().env)) {
    console.log(JSON.stringify({ profile: 'failover', name: 'Failover', gateway: loadSettings().env.ANTHROPIC_BASE_URL }, null, 2))
    return
  }
  const profile = data.profiles[data.active]
  // ponytail: no real traffic tracking — add when gateway exposes usage API
  console.log(JSON.stringify({
    profile: data.active,
    name: profile.name,
    gateway: profile.env.ANTHROPIC_BASE_URL || 'default',
    models: {
      opus: profile.env.ANTHROPIC_DEFAULT_OPUS_MODEL || 'default',
      sonnet: profile.env.ANTHROPIC_DEFAULT_SONNET_MODEL || 'default',
      haiku: profile.env.ANTHROPIC_DEFAULT_HAIKU_MODEL || 'default'
    }
  }, null, 2))
}

function addProfile(key, name, baseUrl, token) {
  if ([NATIVE_KEY, 'native'].includes(key)) { console.error(`"${key}" is reserved`); process.exit(1) }
  const data = loadProfiles()
  data.profiles[key] = {
    name,
    env: {
      ANTHROPIC_BASE_URL: baseUrl,
      ANTHROPIC_AUTH_TOKEN: token
    }
  }
  saveProfiles(data)
  console.log(`✓ Added profile: ${key} (${name})`)
}

// claude-switch models <key> [latest] [prefix=cc/] [opus=..] [sonnet=..] [haiku=..] [main=..] [subagent=..]
// The prefix is only added to bare claude-* names; empty value (opus=) removes the slot.
function setModels(key, assignments) {
  const data = loadProfiles()
  const profile = data.profiles[key]
  if (!profile) { console.error(`Profile "${key}" not found`); process.exit(1) }
  const wasActive = data.active === key && !isNativeActive(data, loadSettings().env)
  const values = {}
  let prefix = profile.modelPrefix ?? ''
  for (const a of assignments) {
    if (a === 'latest') { Object.assign(values, LATEST_CLAUDE); continue }
    const i = a.indexOf('=')
    const slot = i > 0 ? a.slice(0, i) : ''
    if (slot === 'prefix') { prefix = a.slice(i + 1); continue }
    if (!MODEL_SLOTS[slot]) { console.error(`Unknown "${a}". Slots: prefix, ${Object.keys(MODEL_SLOTS).join(', ')}, latest`); process.exit(1) }
    values[slot] = a.slice(i + 1).trim()
  }
  const removed = []
  for (const [slot, v] of Object.entries(values)) {
    const envKey = MODEL_SLOTS[slot]
    if (!v) { delete profile.env[envKey]; removed.push(envKey); continue }
    profile.env[envKey] = v.startsWith('claude-') ? prefix + v : v
  }
  if (prefix) profile.modelPrefix = prefix; else delete profile.modelPrefix
  saveProfiles(data)
  for (const [slot, envKey] of Object.entries(MODEL_SLOTS)) {
    if (profile.env[envKey]) console.log(`  ${slot.padEnd(9)} ${profile.env[envKey]}`)
  }
  if (wasActive) {
    // a removed slot may no longer be managed by any profile: drop it explicitly
    const settings = loadSettings()
    for (const k of removed) delete settings.env?.[k]
    saveSettings(settings)
    applyProfile(key)
  }
}

function removeProfile(key) {
  const data = loadProfiles()
  if (!data.profiles[key]) { console.error(`Profile "${key}" not found`); process.exit(1) }
  if (data.active === key && !isNativeActive(data, loadSettings().env)) { console.error(`Cannot remove active profile. Switch first.`); process.exit(1) }
  delete data.profiles[key]
  saveProfiles(data)
  console.log(`✓ Removed profile: ${key}`)
}

function removeAll() {
  restoreNative()
  saveProfiles({ active: '', profiles: {} })
  console.log('✓ Removed all profiles')
}

function importFromSettings() {
  const settings = loadSettings()
  if (!settings.env?.ANTHROPIC_BASE_URL) { console.error('No gateway config in current settings'); process.exit(1) }
  const key = `imported-${Date.now()}`
  const data = loadProfiles()
  const env = {}
  for (const [k, v] of Object.entries(settings.env)) {
    if (isManaged(k, new Set(EXTRA_MANAGED))) env[k] = v
  }
  data.profiles[key] = { name: `Imported ${new Date().toLocaleDateString()}`, env }
  saveProfiles(data)
  console.log(`✓ Imported current settings as profile: ${key}`)
}

function exportBackup(outPath, flags = []) {
  const includeSecrets = flags.includes('--with-secrets')
  const dest = outPath || join(__dirname, `claude-antigravity-backup-${Math.floor(Date.now() / 1000)}.json`)
  const profiles = existsSync(PROFILES_PATH) ? JSON.parse(readFileSync(PROFILES_PATH, 'utf8')) : { active: '', profiles: {} }
  if (!includeSecrets) {
    for (const p of Object.values(profiles.profiles || {})) {
      delete p.env?.ANTHROPIC_AUTH_TOKEN
      delete p.env?.ANTHROPIC_API_KEY
      delete p.env?.ANTHROPIC_CUSTOM_HEADERS
    }
  }
  const antigravity = existsSync(ANTIGRAVITY_PATH)
    ? JSON.parse(readFileSync(ANTIGRAVITY_PATH, 'utf8'))
    : { active: '', accounts: [] }
  const bundle = {
    format: BACKUP_FORMAT,
    version: BACKUP_VERSION,
    exportedAt: Math.floor(Date.now() / 1000),
    includeSecrets,
    profiles,
    antigravity
  }
  mkdirSync(dirname(dest), { recursive: true })
  writeFileSync(dest, JSON.stringify(bundle, null, 2) + '\n', includeSecrets ? { mode: 0o600 } : undefined)
  if (includeSecrets) chmodSync(dest, 0o600)
  console.log(`✓ Exported backup to ${dest} (${includeSecrets ? 'with secrets, 0600' : 'sanitized'})`)
  console.log(`  Claude profiles: ${Object.keys(profiles.profiles || {}).length} · Antigravity accounts: ${(antigravity.accounts || []).length}`)
}

function importBackup(srcPath, flags = []) {
  if (!srcPath || !existsSync(srcPath)) { console.error(`File not found: ${srcPath || '(missing path)'}`); process.exit(1) }
  const replace = flags.includes('--replace')
  const val = JSON.parse(readFileSync(srcPath, 'utf8'))

  const mergeProfiles = (imp) => {
    if (replace || !existsSync(PROFILES_PATH)) { saveProfiles(imp); return Object.keys(imp.profiles || {}).length }
    const cur = loadProfiles()
    for (const [k, p] of Object.entries(imp.profiles || {})) {
      const existing = cur.profiles[k]
      if (existing?.env) {
        for (const sk of ['ANTHROPIC_AUTH_TOKEN', 'ANTHROPIC_API_KEY', 'ANTHROPIC_CUSTOM_HEADERS']) {
          if (!p.env?.[sk] && existing.env[sk]) p.env = { ...p.env, [sk]: existing.env[sk] }
        }
      }
      cur.profiles[k] = p
    }
    if (!cur.active && imp.active) cur.active = imp.active
    saveProfiles(cur)
    return Object.keys(imp.profiles || {}).length
  }

  const mergeAntigravity = (imp) => {
    if (replace || !existsSync(ANTIGRAVITY_PATH)) {
      writeFileSync(ANTIGRAVITY_PATH, JSON.stringify(imp, null, 2) + '\n')
      return (imp.accounts || []).length
    }
    const cur = JSON.parse(readFileSync(ANTIGRAVITY_PATH, 'utf8'))
    cur.accounts ||= []
    for (const acc of imp.accounts || []) {
      const idx = cur.accounts.findIndex(x => x.key === acc.key)
      if (idx >= 0) cur.accounts[idx] = { ...cur.accounts[idx], ...acc, savedAt: acc.savedAt || cur.accounts[idx].savedAt || 0 }
      else cur.accounts.push(acc)
    }
    if (!cur.active && imp.active) cur.active = imp.active
    writeFileSync(ANTIGRAVITY_PATH, JSON.stringify(cur, null, 2) + '\n')
    return (imp.accounts || []).length
  }

  if (val.format === BACKUP_FORMAT) {
    const cp = val.profiles ? mergeProfiles(val.profiles) : 0
    const ag = val.antigravity ? mergeAntigravity(val.antigravity) : 0
    console.log(`✓ Imported full backup (${replace ? 'replace' : 'merge'}): ${cp} Claude profiles, ${ag} Antigravity accounts`)
    return
  }
  if (Array.isArray(val.accounts)) {
    const ag = mergeAntigravity(val)
    console.log(`✓ Imported antigravity.json (${replace ? 'replace' : 'merge'}): ${ag} accounts`)
    return
  }
  if (val.profiles && typeof val.profiles === 'object') {
    const cp = mergeProfiles(val)
    console.log(`✓ Imported profiles.json (${replace ? 'replace' : 'merge'}): ${cp} profiles`)
    return
  }
  console.error('Unrecognized backup format (expected Full Backup, profiles.json, or antigravity.json)')
  process.exit(1)
}

// --- CLI ---
const [,, cmd, ...args] = process.argv

switch (cmd) {
  case 'list':   case 'ls':    listProfiles(); break
  case 'use':    case 'switch': applyProfile(args[0]); break
  case 'current': case 'status': showCurrent(); break
  case 'add':     addProfile(args[0], args[1] || args[0], args[2], args[3]); break
  case 'models':  setModels(args[0], args.slice(1)); break
  case 'remove':  case 'rm':   removeProfile(args[0]); break
  case 'restore': case 'native': restoreNative(); break
  case 'import':  importFromSettings(); break
  case 'backup':  case 'export': exportBackup(args.find(a => !a.startsWith('--')), args); break
  case 'restore-backup': case 'import-backup': importBackup(args.find(a => !a.startsWith('--')), args); break
  case 'remove-all': removeAll(); break
  case 'json':    showCurrent(); break // for menubar
  default:
    console.log(`Usage: claude-switch <command>

Commands:
  list|ls              List all profiles
  use|switch <key>     Switch to profile (\`use native\` = back to Claude login)
  restore|native       Restore the machine's own Claude Code login
  current|status       Show active profile
  add <key> <name> <url> <token>   Add profile
  models <key> [latest] [prefix=cc/] [opus=..] [sonnet=..] [haiku=..] [main=..] [subagent=..]
                       Set model names (prefix only applies to bare claude-* names;
                       \`latest\` = Opus 5.5 / Sonnet 5.5 / Haiku 5.5; \`opus=\` clears a slot)
  remove|rm <key>      Remove profile
  import               Import current settings as profile
  backup [file] [--with-secrets]   Export portable JSON backup (sanitized by default)
  restore-backup <file> [--replace] Import backup/profiles.json/antigravity.json (merges by default)
  remove-all           Delete all profiles + restore the original Claude login
  json                 Output current profile as JSON (for menubar)`)
}