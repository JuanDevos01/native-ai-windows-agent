/**
 * WhatsApp client wrapper using Baileys.
 *
 * Manages connection to WhatsApp via the multi-device protocol,
 * handles QR authentication, message extraction, and auto-reconnect.
 */
/** Message received from WhatsApp, forwarded to the Rust bot. */
export interface InboundMessage {
    id: string;
    sender: string;
    pn: string;
    content: string;
    timestamp: number;
    isGroup: boolean;
}
/** Options for creating the WhatsApp client. */
export interface WhatsAppClientOptions {
    /** Directory to store auth credentials. */
    authDir: string;
    /** Callback when a message is received. */
    onMessage: (msg: InboundMessage) => void;
    /** Callback when a QR code is generated. */
    onQR: (qr: string) => void;
    /** Callback for connection status changes. */
    onStatus: (status: string) => void;
}
export declare class WhatsAppClient {
    private sock;
    private options;
    private reconnecting;
    constructor(options: WhatsAppClientOptions);
    /** Connect to WhatsApp using Baileys. */
    connect(): Promise<void>;
    /**
     * Extract the human-readable text from a Baileys message object.
     * Handles plain text, extended text (links/replies), media captions,
     * and voice messages.
     */
    private extractMessageContent;
    /** Send a text message to a WhatsApp JID. */
    sendMessage(to: string, text: string): Promise<void>;
    /** Gracefully disconnect from WhatsApp. */
    disconnect(): Promise<void>;
}
