import { useEffect, useState } from "react";

import {
  buildConversationDetailView,
  buildConversationListSections,
} from "../../src/collaboration/view-model.js";
import type {
  CollaborationConversation,
  CollaborationMessage,
  CollaborationParticipant,
  CollaborationTask,
  ConversationListItem,
  ConversationListSection,
  ConversationTimelineItem,
} from "../../src/collaboration/view-model.js";
import { createMockCollaborationState } from "./mock-data.js";
import "./styles.css";

const fallbackState = createMockCollaborationState();

type InboxKind = ConversationListSection["kind"];

function findConversation(conversations: CollaborationConversation[], conversationId: string): CollaborationConversation {
  const conversation = conversations.find((item) => item.id === conversationId);
  if (!conversation) {
    throw new Error(`Conversation "${conversationId}" was not found.`);
  }

  return conversation;
}

function findTask(tasks: CollaborationTask[], taskId: string | undefined): CollaborationTask | undefined {
  if (!taskId) {
    return undefined;
  }

  return tasks.find((item) => item.id === taskId);
}

function getLinkedConversations(
  conversations: CollaborationConversation[],
  selectedConversationId: string,
  task: CollaborationTask | undefined,
): CollaborationConversation[] {
  if (!task) {
    return [];
  }

  const linkedIds = new Set([task.primaryConversationId, ...task.internalConversationIds]);
  linkedIds.delete(selectedConversationId);

  return conversations.filter((conversation) => linkedIds.has(conversation.id));
}

function getConversationMessages(
  messages: CollaborationMessage[],
  conversationId: string,
): CollaborationMessage[] {
  return messages.filter((message) => message.conversationId === conversationId);
}

function getParticipant(participants: CollaborationParticipant[], participantId: string): CollaborationParticipant | undefined {
  return participants.find((participant) => participant.id === participantId);
}

function getStatusTone(status: CollaborationTask["status"] | undefined): string {
  switch (status) {
    case "running":
      return "teal";
    case "waiting-approval":
      return "amber";
    case "blocked":
      return "rose";
    case "completed":
      return "green";
    default:
      return "slate";
  }
}

function getMessageTone(item: ConversationTimelineItem["kind"]): string {
  switch (item) {
    case "status":
      return "teal";
    case "approval":
      return "amber";
    case "handoff":
      return "violet";
    case "result":
      return "indigo";
    default:
      return "neutral";
  }
}

function initialsFor(participant: CollaborationParticipant | undefined): string {
  const fallback = participant?.kind === "system" ? "SY" : "AG";
  if (!participant) {
    return fallback;
  }

  const parts = participant.displayName.split(" ");
  if (parts.length === 1) {
    return participant.displayName.slice(0, 2).toUpperCase();
  }

  return `${parts[0]?.[0] ?? ""}${parts[1]?.[0] ?? ""}`.toUpperCase();
}

function sectionLabel(kind: InboxKind): string {
  switch (kind) {
    case "dm":
      return "Direct Messages";
    case "channel":
      return "Channels";
    case "agent":
      return "Agent Dialogues";
  }
}

function visibilityLabel(visibility: CollaborationConversation["visibility"]): string {
  switch (visibility) {
    case "private":
      return "Private";
    case "shared":
      return "Shared";
    case "internal":
      return "Internal";
  }
}

function visibilityDescription(conversation: CollaborationConversation): string {
  if (conversation.visibility === "private") {
    return "Private DM. This surface can use private memory and draft ideas before they go public.";
  }

  if (conversation.visibility === "internal") {
    return "Internal agent dialogue. This stream is visible in the UI, but it does not auto-post into the public thread.";
  }

  return "Shared channel thread. This surface stays team-facing and only uses shared project memory.";
}

function visibilityTone(visibility: CollaborationConversation["visibility"]): string {
  switch (visibility) {
    case "private":
      return "amber";
    case "shared":
      return "teal";
    case "internal":
      return "violet";
  }
}

interface InboxSectionProps {
  section: ConversationListSection;
  selectedConversationId: string;
  onSelect: (conversationId: string) => void;
}

function InboxSection(props: InboxSectionProps) {
  if (props.section.items.length === 0) {
    return null;
  }

  return (
    <section className="inbox-section">
      <div className="inbox-section__label">{sectionLabel(props.section.kind)}</div>
      <div className="inbox-section__items">
        {props.section.items.map((item) => (
          <ConversationListButton
            key={item.conversationId}
            item={item}
            selected={item.conversationId === props.selectedConversationId}
            onSelect={props.onSelect}
          />
        ))}
      </div>
    </section>
  );
}

interface ConversationListButtonProps {
  item: ConversationListItem;
  selected: boolean;
  onSelect: (conversationId: string) => void;
}

function ConversationListButton(props: ConversationListButtonProps) {
  return (
    <button
      className={`conversation-list-item${props.selected ? " conversation-list-item--selected" : ""}`}
      onClick={() => props.onSelect(props.item.conversationId)}
      type="button"
    >
      <div className="conversation-list-item__header">
        <div className="conversation-list-item__title">{props.item.title}</div>
        <div className="conversation-list-item__time">{formatTime(props.item.latestMessageAt)}</div>
      </div>
      <div className="conversation-list-item__preview">{props.item.latestPreview}</div>
      <div className="conversation-list-item__footer">
        <span className="conversation-list-item__subtitle">{props.item.subtitle}</span>
        {props.item.taskStatus ? (
          <span className={`status-pill status-pill--${getStatusTone(props.item.taskStatus)}`}>
            {props.item.taskStatus}
          </span>
        ) : null}
        {props.item.unreadCount > 0 ? (
          <span className="unread-pill">{props.item.unreadCount}</span>
        ) : null}
      </div>
    </button>
  );
}

function formatTime(timestamp: string): string {
  return new Date(timestamp).toLocaleTimeString([], {
    hour: "2-digit",
    minute: "2-digit",
  });
}

export default function App() {
  const [collaborationState, setCollaborationState] = useState(fallbackState);
  const [selectedConversationId, setSelectedConversationId] = useState("channel-design-thread");
  const [dataSource, setDataSource] = useState<"sqlite" | "demo">("demo");

  useEffect(() => {
    let cancelled = false;

    async function loadSnapshot(): Promise<void> {
      try {
        const response = await fetch("/api/collaboration");
        if (!response.ok) {
          throw new Error(`Unexpected response status ${response.status}`);
        }

        const payload = (await response.json()) as typeof fallbackState;
        if (cancelled) {
          return;
        }

        setCollaborationState(payload);
        setDataSource("sqlite");
      } catch {
        if (cancelled) {
          return;
        }

        setCollaborationState(fallbackState);
        setDataSource("demo");
      }
    }

    void loadSnapshot();

    return () => {
      cancelled = true;
    };
  }, []);

  useEffect(() => {
    const exists = collaborationState.conversations.some(
      (conversation) => conversation.id === selectedConversationId,
    );
    if (!exists && collaborationState.conversations[0]) {
      setSelectedConversationId(collaborationState.conversations[0].id);
    }
  }, [collaborationState, selectedConversationId]);

  const sections = buildConversationListSections({
    conversations: collaborationState.conversations,
    participants: collaborationState.participants,
    messages: collaborationState.messages,
    tasks: collaborationState.tasks,
    unreadCounts: collaborationState.unreadCounts,
  });

  const selectedConversation = findConversation(collaborationState.conversations, selectedConversationId);
  const selectedTask = findTask(collaborationState.tasks, selectedConversation.taskId);
  const linkedConversations = getLinkedConversations(
    collaborationState.conversations,
    selectedConversation.id,
    selectedTask,
  );
  const selectedMessages = getConversationMessages(collaborationState.messages, selectedConversation.id);
  const detail = buildConversationDetailView({
    conversation: selectedConversation,
    participants: collaborationState.participants,
    messages: selectedMessages,
    task: selectedTask,
    linkedConversations,
  });

  return (
    <div className="app-shell">
      <aside className="workspace-rail" aria-label="Workspace switcher">
        <div className="workspace-rail__logo">O</div>
        <div className="workspace-rail__icon workspace-rail__icon--active">D</div>
        <div className="workspace-rail__icon">C</div>
        <div className="workspace-rail__icon">A</div>
        <div className="workspace-rail__footer">YT</div>
      </aside>

      <section className="inbox-pane">
        <header className="pane-header pane-header--stacked">
          <div>
            <div className="pane-header__title">Otto</div>
            <div className="pane-header__subtitle">
              Slack-like collaboration preview
            </div>
          </div>
          <div className="data-source-pill">
            {dataSource === "sqlite" ? "SQLite live data" : "Bundled demo data"}
          </div>
          <div className="search-box">Search conversations</div>
        </header>

        <div className="inbox-pane__body">
          {sections.map((section) => (
            <InboxSection
              key={section.kind}
              section={section}
              selectedConversationId={selectedConversationId}
              onSelect={setSelectedConversationId}
            />
          ))}
        </div>
      </section>

      <main className="thread-pane">
        <header className="pane-header">
          <div>
            <div className="pane-header__title">{detail.title}</div>
            <div className="pane-header__subtitle">{detail.subtitle}</div>
          </div>
          {selectedTask ? (
            <div className={`status-pill status-pill--${getStatusTone(selectedTask.status)}`}>
              {selectedTask.status}
            </div>
          ) : null}
        </header>

        {linkedConversations.length > 0 ? (
          <div className="linked-tabs">
            <button className="linked-tab linked-tab--selected" type="button">
              {detail.title}
            </button>
            {linkedConversations.map((conversation) => (
              <button
                key={conversation.id}
                className="linked-tab"
                onClick={() => setSelectedConversationId(conversation.id)}
                type="button"
              >
                {conversation.title}
              </button>
            ))}
          </div>
        ) : null}

        <div className={`visibility-banner visibility-banner--${visibilityTone(selectedConversation.visibility)}`}>
          <div className="visibility-banner__label">
            {visibilityLabel(selectedConversation.visibility)}
          </div>
          <div className="visibility-banner__text">
            {visibilityDescription(selectedConversation)}
          </div>
        </div>

        <div className="thread-compose thread-compose--disabled">Read-only preview. Reply in thread...</div>

        <div className="thread-timeline">
          {detail.timeline.map((item) => {
            const participant = getParticipant(collaborationState.participants, item.laneParticipantId);
            return (
              <article key={item.id} className="thread-message">
                <div className={`thread-message__avatar thread-message__avatar--${participant?.kind ?? "agent"}`}>
                  {initialsFor(participant)}
                </div>
                <div className="thread-message__content">
                  <div className="thread-message__meta">
                    <span className="thread-message__author">{participant?.displayName ?? item.laneParticipantId}</span>
                    <span className="thread-message__time">{formatTime(item.createdAt)}</span>
                    {item.badge ? (
                      <span className={`message-pill message-pill--${getMessageTone(item.kind)}`}>
                        {item.badge}
                      </span>
                    ) : null}
                  </div>
                  <div className="thread-message__body">{item.body}</div>
                </div>
              </article>
            );
          })}
        </div>
      </main>

      <aside className="detail-pane">
        <header className="pane-header">
          <div>
            <div className="pane-header__title">{selectedTask?.id ?? "Conversation"}</div>
            <div className="pane-header__subtitle">Thread details</div>
          </div>
        </header>

        <div className="detail-card">
          <div className="detail-card__label">Current conversation</div>
          <div className="detail-card__row">
            <span>Mode</span>
            <strong>{detail.subtitle}</strong>
          </div>
          <div className="detail-card__row">
            <span>Visibility</span>
            <strong>{visibilityLabel(selectedConversation.visibility)}</strong>
          </div>
        </div>

        <div className="detail-card">
          <div className="detail-card__label">Status</div>
          {selectedTask ? (
            <>
              <div className={`status-pill status-pill--${getStatusTone(selectedTask.status)}`}>
                {selectedTask.status}
              </div>
              <div className="detail-card__row">
                <span>Owner</span>
                <strong>{detail.activeTask?.ownerAgentId}</strong>
              </div>
              <div className="detail-card__row">
                <span>Runtime</span>
                <strong>{selectedTask.runtimeTarget}</strong>
              </div>
            </>
          ) : (
            <div className="detail-card__text">This conversation is not attached to an active task.</div>
          )}
        </div>

        <div className="detail-card">
          <div className="detail-card__label">Linked conversations</div>
          {linkedConversations.length === 0 ? (
            <div className="detail-card__text">No linked conversations for this task.</div>
          ) : (
            <div className="linked-conversation-list">
              {linkedConversations.map((conversation) => (
                <button
                  key={conversation.id}
                  className="linked-conversation-button"
                  onClick={() => setSelectedConversationId(conversation.id)}
                  type="button"
                >
                  <span>{conversation.title}</span>
                  <span className="linked-conversation-button__meta">
                    {conversation.visibility}
                  </span>
                </button>
              ))}
            </div>
          )}
        </div>

        <div className="detail-card">
          <div className="detail-card__label">Memory access</div>
          <div className="detail-card__text">DM can use private memory.</div>
          <div className="detail-card__text">Channel uses shared project memory only.</div>
          <div className="detail-card__text">Internal agent dialogue stays off-channel.</div>
        </div>

        <div className="detail-card">
          <div className="detail-card__label">Participants</div>
          <div className="participant-list">
            {detail.lanes.map((lane) => (
              <div key={lane.participantId} className="participant-list__item">
                <span>{lane.label}</span>
                <span className="participant-list__meta">{lane.kind}</span>
              </div>
            ))}
          </div>
        </div>
      </aside>
    </div>
  );
}
