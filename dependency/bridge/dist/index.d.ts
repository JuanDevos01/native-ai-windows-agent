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
export {};
