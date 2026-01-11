/**
 * Network module exports.
 * 
 * Note: WebSocketServer is NOT exported here - it's built separately
 * and dynamically imported on desktop only.
 */

export { SyncWebSocketClient } from "./WebSocketClient";
export { PeerManager, type PeerInfo } from "./PeerManager";
