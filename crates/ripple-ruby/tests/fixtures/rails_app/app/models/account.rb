require_relative "../../lib/sanitizer"
require "tag_parser"
require "json"

class Account < ApplicationRecord
  include Searchable

  has_many :statuses, dependent: :destroy
  has_one :profile, class_name: "Admin::Profile"
  belongs_to :owner, polymorphic: true
  has_many :things, class_name: thing_class_name
  has_many :followers, through: :follows, source: :account

  def visibility_levels
    Status::VISIBILITY
  end

  def missing_constant
    MediaAttachment::NOT_DEFINED
  end

  def dynamic
    "MediaAttachment".constantize
    Object.const_get("Shared")
  end

  def gem_constant
    Nokogiri::HTML
  end
end
