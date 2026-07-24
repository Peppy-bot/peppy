import { glob, readFile } from 'node:fs/promises';
import { basename, extname, isAbsolute, relative, resolve, sep } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import type { Loader, LoaderContext } from 'astro/loaders';
import { XMLParser } from 'fast-xml-parser';
import { SyntaxValidator } from 'fast-xml-validator';

type ReleaseData = {
  [key: string]: unknown;
  version: string;
  date: Date;
  updated?: Date;
  description: string;
};

type ParsedRelease = {
  data: ReleaseData;
  body: string;
};

type XmlElement = Record<string, unknown>;

const atomParser = new XMLParser({
  ignoreAttributes: false,
  parseTagValue: false,
  processEntities: true,
  trimValues: false,
});
const atomValidator = new SyntaxValidator({ multipleRoots: false });

function parseElement(value: unknown, elementName: string, filePath: string): XmlElement {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) {
    throw new Error(`Expected one <${elementName}> element in ${filePath}`);
  }

  return value as XmlElement;
}

function readElementText(element: XmlElement, elementName: string, filePath: string): string {
  const value = element[elementName];
  if (value === undefined) return '';
  if (typeof value === 'string') return value.trim();

  const child = parseElement(value, elementName, filePath);
  const text = child['#text'];
  if (text === undefined && Object.keys(child).every((key) => key.startsWith('@_'))) {
    return '';
  }
  if (typeof text !== 'string') {
    throw new Error(`Expected text content in <${elementName}> in ${filePath}`);
  }

  return text.trim();
}

function parseDate(value: string, elementName: string, filePath: string): Date {
  const date = new Date(value);
  if (!value || Number.isNaN(date.valueOf())) {
    throw new Error(`Invalid Atom ${elementName} date "${value}" in ${filePath}`);
  }

  return date;
}

function versionFromFilePath(filePath: string): string {
  const extension = extname(filePath);
  const version = basename(filePath, extension).replace(/^v/i, '');
  if (!version) {
    throw new Error(`Could not derive a release version from ${filePath}`);
  }

  return version;
}

function releaseBody(contentHtml: string): string {
  const articleMatch = contentHtml.match(/<article\b[^>]*>([\s\S]*?)<\/article>/i);
  if (!articleMatch) return contentHtml.trimStart();

  const articleInner = articleMatch[1] ?? '';
  const headerEndIndex = articleInner.toLowerCase().indexOf('</header>');
  const body =
    headerEndIndex === -1
      ? articleInner
      : articleInner.slice(headerEndIndex + '</header>'.length);

  return body.trimStart();
}

function invalidXmlError(filePath: string, failure: unknown): Error {
  const failureRecord =
    typeof failure === 'object' && failure !== null ? (failure as Record<string, unknown>) : undefined;
  const nestedFailure =
    typeof failureRecord?.err === 'object' && failureRecord.err !== null
      ? (failureRecord.err as Record<string, unknown>)
      : failureRecord;
  const line = typeof nestedFailure?.line === 'number' ? nestedFailure.line : undefined;
  const column = typeof nestedFailure?.col === 'number' ? nestedFailure.col : undefined;
  const location = line === undefined || column === undefined ? '' : ` at line ${line}, column ${column}`;
  let detail = String(failure);
  if (failure instanceof Error) detail = failure.message;
  if (typeof nestedFailure?.msg === 'string') detail = nestedFailure.msg;

  return new Error(`Invalid Atom XML in ${filePath}${location}: ${detail}`, {
    cause: failure,
  });
}

export function parseReleaseHtml(source: string, fileUrl: URL): ParsedRelease {
  const filePath = fileURLToPath(fileUrl);
  try {
    atomValidator.validate(source);
  } catch (error) {
    throw invalidXmlError(filePath, error);
  }

  const document = parseElement(atomParser.parse(source), 'document', filePath);
  const entry = parseElement(document.entry, 'entry', filePath);
  const publishedText = readElementText(entry, 'published', filePath);
  const updatedText = readElementText(entry, 'updated', filePath);
  const contentHtml = readElementText(entry, 'content', filePath);
  const body = releaseBody(contentHtml);

  let description = readElementText(entry, 'summary', filePath);
  if (!description && contentHtml) {
    const emphasisMatch = contentHtml.match(/<em>([\s\S]*?)<\/em>/i);
    description = emphasisMatch?.[1]?.trim() ?? '';
  }

  const data: ReleaseData = {
    version: versionFromFilePath(filePath),
    date: parseDate(publishedText || updatedText, publishedText ? 'published' : 'updated', filePath),
    description,
  };

  if (updatedText) {
    data.updated = parseDate(updatedText, 'updated', filePath);
  }

  return { data, body };
}

function normalizePath(filePath: string): string {
  return filePath.split(sep).join('/');
}

function entryId(entryPath: string): string {
  return normalizePath(entryPath).replace(/\.html$/, '');
}

function entryPathFromChange(changedPath: string, basePath: string): string | undefined {
  const entryPath = relative(basePath, changedPath);
  if (isAbsolute(entryPath) || entryPath.startsWith(`..${sep}`) || !entryPath.endsWith('.html')) {
    return undefined;
  }

  return entryPath;
}

async function syncEntry(
  entryPath: string,
  basePath: string,
  context: LoaderContext
): Promise<string> {
  const absolutePath = resolve(basePath, entryPath);
  const fileUrl = pathToFileURL(absolutePath);
  const source = await readFile(fileUrl, 'utf8');
  const parsedRelease = parseReleaseHtml(source, fileUrl);
  const id = entryId(entryPath);
  const filePath = normalizePath(relative(fileURLToPath(context.config.root), absolutePath));
  const data = await context.parseData({
    id,
    data: parsedRelease.data,
    filePath,
  });

  context.store.set({
    id,
    data,
    body: parsedRelease.body,
    digest: context.generateDigest(source),
    filePath,
  });

  return id;
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

export function releaseHtmlLoader(base = './src/content/releases'): Loader {
  return {
    name: 'release-html-loader',
    async load(context) {
      const baseUrl = new URL(base.endsWith('/') ? base : `${base}/`, context.config.root);
      const basePath = fileURLToPath(baseUrl);
      const releasePaths = [];

      for await (const releasePath of glob('**/*.html', { cwd: basePath })) {
        releasePaths.push(releasePath);
      }
      releasePaths.sort();

      const staleIds = new Set(context.store.keys());
      for (const releasePath of releasePaths) {
        const id = await syncEntry(releasePath, basePath, context);
        staleIds.delete(id);
      }
      for (const id of staleIds) {
        context.store.delete(id);
      }

      if (!context.watcher) return;

      context.watcher.add(basePath);
      const reload = async (changedPath: string) => {
        const releasePath = entryPathFromChange(changedPath, basePath);
        if (!releasePath) return;

        try {
          await syncEntry(releasePath, basePath, context);
          context.logger.info(`Reloaded release from ${releasePath}`);
        } catch (error) {
          context.logger.error(`Failed to reload ${releasePath}: ${errorMessage(error)}`);
        }
      };

      context.watcher.on('add', reload);
      context.watcher.on('change', reload);
      context.watcher.on('unlink', (changedPath) => {
        const releasePath = entryPathFromChange(changedPath, basePath);
        if (!releasePath) return;

        context.store.delete(entryId(releasePath));
      });
    },
  };
}
