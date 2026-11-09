Fixed

- **A refused JSON-RPC request gets its 401, not a connection reset.**
  The 401 path dropped the request body unread. When the body arrived
  after the headers, the close reset the connection, and the client could
  read `ECONNRESET` instead of the 401. mempool re-reads a rotated cookie
  only on that 401. The node still answers 401 at once, then reads and
  drops the rest of the body before it closes, up to the 32 MiB RPC body
  cap and the 30 s read timeout.
