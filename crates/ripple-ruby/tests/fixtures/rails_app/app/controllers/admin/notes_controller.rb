module Admin
  class NotesController < ApplicationController
    def index
      @notes = Note.all
      @profile = Admin::Profile.first
    end

    def lookup
      Admin.const_get("Person")
      "TagParser".safe_constantize
    end
  end
end
