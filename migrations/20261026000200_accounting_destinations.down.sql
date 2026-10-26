-- Export integration history before rollback. This never changes external books.
DROP TABLE accounting.postings;
DROP TABLE accounting.books;
DROP FUNCTION accounting.postings_terminal_immutable();
DROP SCHEMA accounting;
