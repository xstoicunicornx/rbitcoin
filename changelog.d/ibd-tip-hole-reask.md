Fixed

- **IBD no longer re-requests a tip-hole block from a peer that still owes
  it.** Dropping a tip-hole owner forgot the getdata it was sent, so the
  next assign pass asked the same peer again. Peers answer every getdata,
  so on signet with 30 peers they spent 77–85% of upload re-sending blocks
  already stored, and tip+1 waited behind those copies for up to 55s. A
  dropped owner now keeps the request and is not asked for that hash again.
- **A tip-hole owner with other getdata queued is dropped only when it
  is actually slow.** Every peer has several blocks in flight during IBD,
  so the owner was dropped and replaced on every 50ms assign pass. It is
  now dropped only after holding the hash for 5s, and only when a free
  peer's expected drain time is at most half the owner's.
