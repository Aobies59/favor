-- Weekday a weekly task's 7-day window starts on (0 = Monday … 6 = Sunday).
-- NULL: the window starts whenever the task was dealt (tasks from before this).
ALTER TABLE tasks ADD COLUMN start_dow INT CHECK (start_dow BETWEEN 0 AND 6);
