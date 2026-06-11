-- Operator-written annotation on an incident ("fiber cut"), set via
-- `hora annotate`, shown on /history and in the Atom feed.
ALTER TABLE incidents ADD COLUMN note TEXT;
