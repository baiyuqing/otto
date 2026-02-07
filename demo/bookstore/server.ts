import http from "http";
import { IncomingMessage, ServerResponse } from "http";
import url from "url";

type Book = { id: string; title: string; author: string; price: number; stock: number };
type Preorder = { id: string; bookId: string; email: string; createdAt: string };

const books: Record<string, Book> = {
  "1": { id: "1", title: "The Pragmatic Programmer", author: "Andrew Hunt", price: 42, stock: 5 },
  "2": { id: "2", title: "Designing Data-Intensive Applications", author: "Martin Kleppmann", price: 55, stock: 3 },
};

const preorders: Preorder[] = [];

function json(res: ServerResponse, status: number, body: unknown) {
  res.writeHead(status, { "Content-Type": "application/json" });
  res.end(JSON.stringify(body));
}

function notFound(res: ServerResponse) {
  json(res, 404, { error: "not_found" });
}

function badRequest(res: ServerResponse, message: string) {
  json(res, 400, { error: "bad_request", message });
}

function createPreorder(bookId: string, email: string): Preorder {
  if (!books[bookId]) {
    throw new Error("book_not_found");
  }
  const preorder: Preorder = {
    id: `po_${Date.now()}`,
    bookId,
    email,
    createdAt: new Date().toISOString(),
  };
  preorders.push(preorder);
  return preorder;
}

function handleListBooks(res: ServerResponse) {
  json(res, 200, { books: Object.values(books) });
}

async function handlePreorder(req: IncomingMessage, res: ServerResponse) {
  let payload = "";
  for await (const chunk of req) {
    payload += chunk.toString();
  }
  try {
    const body = JSON.parse(payload);
    if (!body.bookId || !body.email) {
      return badRequest(res, "bookId and email are required");
    }
  const preorder = createPreorder(String(body.bookId), String(body.email));
  // TODO: persist to MySQL using a real client (see schema.sql)
  return json(res, 201, { preorder });
} catch (err: any) {
    if (err?.message === "book_not_found") {
      return notFound(res);
    }
    return badRequest(res, "invalid JSON payload");
  }
}

const server = http.createServer(async (req, res) => {
  const { pathname } = url.parse(req.url || "", true);
  if (req.method === "GET" && pathname === "/books") {
    return handleListBooks(res);
  }
  if (req.method === "POST" && pathname === "/preorders") {
    return handlePreorder(req, res);
  }
  notFound(res);
});

const PORT = Number(process.env.PORT || 3000);
server.listen(PORT, () => {
  // eslint-disable-next-line no-console
  console.log(`Bookstore API listening on http://localhost:${PORT}`);
});

// Minimal MySQL wiring notes:
// - Install mysql2: npm install mysql2
// - Create a pool once and use prepared statements for inserts/selects.
// - Schema lives in schema.sql beside this file.
