-- What the service actually answered: status line, headers and the start of
-- the body of the failing HTTP response, captured when the down was confirmed.
-- Bounded at capture time; shown on /history to authorized viewers.
ALTER TABLE incidents ADD COLUMN snapshot TEXT;
