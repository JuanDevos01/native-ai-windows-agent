/**
 * WebSocket server for Metis ↔ bridge communication.
 *
 * The server listens on a configurable port (default 3001).
 * The Rust Metis process connects as a WebSocket **client** and exchanges
 * JSON messages following this protocol:
 *
 * Bridge → Rust:
 *   {"type":"message","id":"...","sender":"...","pn":"...","content":"...","timestamp":N,"isGroup":bool}
 *   {"type":"qr","qr":"..."}
 *   {"type":"status","status":"connected"|"disconnected"}
 *   {"type":"error","error":"..."}
 *
 * Rust → Bridge:
 *   {"type":"send","to":"...","text":"..."}
 *
 * Bridge → Rust (ack):
 *   {"type":"sent","to":"..."}
 *   {"type":"error","error":"..."}
 */
import { WebSocketServer, WebSocket } from 'ws';
import { WhatsAppClient } from './whatsapp.js';
// ─────────────────────────────────────────────
// BridgeServer
// ─────────────────────────────────────────────
export class BridgeServer {
    port;
    authDir;
    wss = null;
    wa = null;
    clients = new Set();
    constructor(port, authDir) {
        this.port = port;
        this.authDir = authDir;
    }
    /** Start the WebSocket server and connect to WhatsApp. */
    async start() {
        this.wss = new WebSocketServer({ port: this.port });
        console.log(`[bridge] 🌉 server listening on ws://localhost:${this.port}`);
        this.wa = new WhatsAppClient({
            authDir: this.authDir,
            onMessage: (msg) => this.broadcast({ type: 'message', ...msg }),
            onQR: (qr) => this.broadcast({ type: 'qr', qr }),
            onStatus: (status) => this.broadcast({ type: 'status', status }),
        });
        this.wss.on('connection', (ws) => {
            console.log('[bridge] 🔗 Metis client connected');
            this.clients.add(ws);
            ws.on('message', async (data) => {
                try {
                    const cmd = JSON.parse(data.toString());
                    await this.handleCommand(cmd, ws);
                }
                catch (error) {
                    console.error('[bridge] error handling command:', error);
                    ws.send(JSON.stringify({ type: 'error', error: String(error) }));
                }
            });
            ws.on('close', () => {
                console.log('[bridge] 🔌 Metis client disconnected');
                this.clients.delete(ws);
            });
            ws.on('error', (error) => {
                console.error('[bridge] client ws error:', error.message);
                this.clients.delete(ws);
            });
        });
        await this.wa.connect();
    }
    /** Handle an outbound command from Metis. */
    async handleCommand(cmd, ws) {
        if (cmd.type !== 'send') {
            ws.send(JSON.stringify({
                type: 'error',
                error: `unknown command type: ${cmd.type}`,
            }));
            return;
        }
        if (!this.wa) {
            ws.send(JSON.stringify({ type: 'error', error: 'WhatsApp not connected' }));
            return;
        }
        await this.wa.sendMessage(cmd.to, cmd.text);
        ws.send(JSON.stringify({ type: 'sent', to: cmd.to }));
    }
    /** Broadcast a bridge event to all connected Metis clients. */
    broadcast(event) {
        const data = JSON.stringify(event);
        for (const client of this.clients) {
            if (client.readyState === WebSocket.OPEN) {
                client.send(data);
            }
        }
    }
    /** Gracefully shut down the bridge. */
    async stop() {
        for (const client of this.clients) {
            client.close();
        }
        this.clients.clear();
        if (this.wss) {
            this.wss.close();
            this.wss = null;
        }
        if (this.wa) {
            await this.wa.disconnect();
            this.wa = null;
        }
    }
}
