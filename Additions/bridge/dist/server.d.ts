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
export declare class BridgeServer {
    private port;
    private authDir;
    private wss;
    private wa;
    private clients;
    constructor(port: number, authDir: string);
    /** Start the WebSocket server and connect to WhatsApp. */
    start(): Promise<void>;
    /** Handle an outbound command from Metis. */
    private handleCommand;
    /** Broadcast a bridge event to all connected Metis clients. */
    private broadcast;
    /** Gracefully shut down the bridge. */
    stop(): Promise<void>;
}
