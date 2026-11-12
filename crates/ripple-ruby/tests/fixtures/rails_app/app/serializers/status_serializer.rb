class StatusSerializer
  has_many :media_attachments, serializer: "MediaAttachmentSerializer"
end
