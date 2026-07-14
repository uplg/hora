-- The multi-vantage verdict ("confirmed down from 2/2 vantage points"),
-- recorded on the incident once the peers answered - so the post-mortem can
-- replay what the mesh saw, not just what this node saw. NULL when no peers
-- were asked (confirmation disabled, or none reachable).
ALTER TABLE incidents ADD COLUMN vantage TEXT;
