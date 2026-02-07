-- Minimal tables to back preorders
CREATE TABLE IF NOT EXISTS books (
  id VARCHAR(64) PRIMARY KEY,
  title VARCHAR(255) NOT NULL,
  author VARCHAR(255) NOT NULL,
  price DECIMAL(10,2) NOT NULL,
  stock INT NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS preorders (
  id VARCHAR(64) PRIMARY KEY,
  book_id VARCHAR(64) NOT NULL,
  email VARCHAR(320) NOT NULL,
  created_at DATETIME NOT NULL,
  FOREIGN KEY (book_id) REFERENCES books(id)
);

-- Seed sample books
INSERT INTO books (id, title, author, price, stock) VALUES
  ('1', 'The Pragmatic Programmer', 'Andrew Hunt', 42.00, 5),
  ('2', 'Designing Data-Intensive Applications', 'Martin Kleppmann', 55.00, 3)
ON DUPLICATE KEY UPDATE title = VALUES(title);
