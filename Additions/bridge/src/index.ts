#!/usr/bin/env node
/**
 * Metis WhatsApp Bridge — entry point.
 *
 * Runs a Node.js sidecar process that speaks the WhatsApp Web protocol
 * via Baileys and exposes a WebSocket server for the Rust Metis process.
 *
 * Environment variables:
 *   BRIDGE_PORT  — WebSocket server port (default: 3001)
 *   AUTH_DIR     — directory to store WhatsApp auth state (default: ~/.metis/whatsapp-auth)
 */

import { webcrypto } from 'crypto';
if (!globalThis.crypto) {
  (globalThis as any).crypto = webcrypto;
}

import { BridgeServer } from './server.js';
import { homedir } from 'os';
import { join } from 'path';

const PORT = parseInt(process.env.BRIDGE_PORT || '3001', 10);
const AUTH_DIR =
  process.env.AUTH_DIR || join(homedir(), '.metis', 'whatsapp-auth');

console.log('🤖 Metis WhatsApp Bridge');
console.log('========================');
console.log(`   port:     ${PORT}`);
console.log(`   auth dir: ${AUTH_DIR}`);
console.log();

const server = new BridgeServer(PORT, AUTH_DIR);

// ── Graceful shutdown ──
const shutdown = async () => {
  console.log('\n[bridge] shutting down …');
  await server.stop();
  process.exit(0);
};

process.on('SIGINT', shutdown);
process.on('SIGTERM', shutdown);

// ── Start ──
server.start().catch((error) => {
  console.error('[bridge] fatal:', error);
  process.exit(1);
});
