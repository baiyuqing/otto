// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react'
import { createElement } from 'react'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { Composer } from './Composer'

const props = () => ({
  disabled: false,
  running: false,
  compacting: false,
  onSend: vi.fn(),
  onCancel: vi.fn(),
  onCompact: vi.fn(),
})

afterEach(cleanup)

describe('Composer image input', () => {
  it('sends one selected image with the prompt', async () => {
    const p = props()
    render(createElement(Composer, p))
    const file = new File([new Uint8Array([137, 80, 78, 71, 13, 10, 26, 10])], 'shot.png', {
      type: 'image/png',
    })

    fireEvent.change(screen.getByLabelText('Attach image'), { target: { files: [file] } })
    fireEvent.change(screen.getByPlaceholderText('Message Otto…'), { target: { value: 'read it' } })
    await waitFor(() => expect(screen.getByText('shot.png')).toBeTruthy())
    fireEvent.click(screen.getByText('Send'))

    expect(p.onSend).toHaveBeenCalledWith('read it', {
      data: 'iVBORw0KGgo=',
      mime_type: 'image/png',
    })
  })

  it('accepts a pasted image', async () => {
    const p = props()
    render(createElement(Composer, p))
    const file = new File([new Uint8Array([255, 216, 255])], 'paste.jpg', {
      type: 'image/jpeg',
    })
    const composer = screen.getByPlaceholderText('Message Otto…')

    fireEvent.paste(composer, {
      clipboardData: {
        files: [],
        items: [{ kind: 'file', type: 'image/jpeg', getAsFile: () => file }],
      },
    })
    await waitFor(() => expect(screen.getByText('paste.jpg')).toBeTruthy())
  })
})
